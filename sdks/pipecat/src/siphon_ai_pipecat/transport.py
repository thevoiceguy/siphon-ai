"""Pipecat transport over a SiphonAI bridge WebSocket (FastAPI/Starlette)."""

from __future__ import annotations

import asyncio
import hmac
from typing import Any

from loguru import logger
from pipecat.frames.frames import (
    CancelFrame,
    EndFrame,
    Frame,
    InputAudioRawFrame,
    InterruptionFrame,
    OutputAudioRawFrame,
    OutputDTMFFrame,
    OutputDTMFUrgentFrame,
    OutputTransportMessageFrame,
    OutputTransportMessageUrgentFrame,
    StartFrame,
)
from pipecat.pipeline.worker import PipelineParams
from pipecat.processors.frame_processor import FrameDirection
from pipecat.transports.base_input import BaseInputTransport
from pipecat.transports.base_output import BaseOutputTransport
from pipecat.transports.base_transport import BaseTransport, TransportParams
from pydantic import field_validator
from siphon_ai_server import (
    SUBPROTOCOL,
    BargeInResolved,
    Dtmf,
    Error,
    Event,
    Mark,
    SpeechStarted,
    Start,
    Stop,
    frame_bytes,
    parse_event,
)

from .frames import SiphonEventFrame
from .serializer import SiphonFrameSerializer

try:
    from fastapi import WebSocket
    from starlette.websockets import WebSocketState
except ModuleNotFoundError as e:  # pragma: no cover - import guard
    raise ImportError(
        'siphon-ai-pipecat needs FastAPI websockets: pip install "pipecat-ai[websocket]"'
    ) from e

__all__ = ["SiphonHandshakeError", "SiphonParams", "SiphonTransport"]

FRAME_SECS = 0.020
# Upper bound on waiting for a pending transfer/park to resolve before the
# end-of-pipeline hangup (a REFER round-trip through a PBX is seconds).
HANDOFF_WAIT_SECS = 10.0
# Mark name the transport uses to learn the caller has heard the last
# frame before it hangs up (PROTOCOL.md §4.2). Server-chosen and opaque.
END_MARK = "pipecat-end"


class SiphonHandshakeError(Exception):
    """The connection was not a usable SiphonAI bridge session; the socket
    has already been closed with the appropriate code."""


class SiphonParams(TransportParams):
    """Transport parameters. Audio in/out are on by default, and the audio
    sample rates are always pinned to the call's negotiated rate
    (``start.audio.sample_rate``) — whatever you pass for them is replaced.

    Parameters:
        playout_lead_ms: How far ahead of real time outbound audio may run
            (event-loop jitter budget). Must stay under SiphonAI's 200 ms
            playout window (PROTOCOL.md §5.5), beyond which the daemon drops
            the oldest audio.
        prime_start_deadline: Send one 20 ms silence frame as soon as the
            pipeline is running, which satisfies the daemon's
            ``server_start_deadline_secs`` (§3.1) for listen-first bots and
            slow cold-start greetings.
        auto_hang_up: Send ``hangup`` when the pipeline ends (``EndFrame``
            / ``CancelFrame``) unless the daemon already ended the call. With
            ``[bridge].ws_reconnect_enabled`` a bare socket close would be
            redialed, not hung up (§5.7).
        end_drain_timeout_secs: On ``EndFrame``, how long to wait for the
            caller to finish *hearing* queued audio before ``hangup``.
        pause_decision_margin_ms: Pause-mode barge-in (§3.2): send
            ``barge_in_reject`` this long before the daemon's
            ``decision_deadline_ms`` if Pipecat hasn't interrupted the bot.
        dtmf_duration_ms: Duration of each digit sent for an
            ``OutputDTMFFrame`` (the daemon clamps to [40, 2000]).
    """

    audio_in_enabled: bool = True
    audio_out_enabled: bool = True
    # One chunk = one 20 ms protocol frame at the call's rate.
    audio_out_10ms_chunks: int = 2
    # Replaced by the mark-drained hangup (see end_drain_timeout_secs).
    audio_out_end_silence_secs: int = 0
    playout_lead_ms: int = 60
    prime_start_deadline: bool = True
    auto_hang_up: bool = True
    end_drain_timeout_secs: float = 2.0
    pause_decision_margin_ms: int = 100
    dtmf_duration_ms: int = 160

    @field_validator("playout_lead_ms")
    @classmethod
    def _lead_within_window(cls, v: int) -> int:
        if not 0 <= v < 200:
            raise ValueError("playout_lead_ms must be in [0, 200) — SiphonAI drops beyond 200 ms")
        return v


class _SiphonSocket:
    """Serialized sends + close bookkeeping over a Starlette WebSocket."""

    def __init__(self, websocket: WebSocket) -> None:
        self._ws = websocket
        self._lock = asyncio.Lock()
        self._failed = False
        self.closed_locally = False

    @property
    def open(self) -> bool:
        return (
            not self._failed
            and not self.closed_locally
            and self._ws.client_state == WebSocketState.CONNECTED
            and self._ws.application_state == WebSocketState.CONNECTED
        )

    async def send(self, data: str | bytes) -> bool:
        if not self.open:
            return False
        try:
            async with self._lock:
                if isinstance(data, (bytes, bytearray)):
                    await self._ws.send_bytes(bytes(data))
                else:
                    await self._ws.send_text(data)
            return True
        except Exception as e:
            self._failed = True
            logger.debug(f"siphon socket send failed: {e.__class__.__name__} ({e})")
            return False

    async def receive(self) -> str | bytes | None:
        """Next message, or ``None`` once the peer has gone."""
        try:
            message = await self._ws.receive()
        except Exception:
            return None
        if message["type"] == "websocket.disconnect":
            return None
        if message.get("bytes") is not None:
            return message["bytes"]
        return message.get("text")

    async def close(self) -> None:
        if self.closed_locally:
            return
        self.closed_locally = True
        if self._ws.application_state != WebSocketState.CONNECTED:
            return
        try:
            await asyncio.wait_for(self._ws.close(code=1000), timeout=1.0)
        except Exception:
            pass


class SiphonInputTransport(BaseInputTransport):
    """Caller audio and protocol events into the pipeline."""

    def __init__(self, transport: SiphonTransport, params: SiphonParams, **kwargs: Any) -> None:
        super().__init__(params, **kwargs)
        self._transport = transport
        self._receive_task: asyncio.Task | None = None
        self._stopping = False

    async def start(self, frame: StartFrame) -> None:
        await super().start(frame)
        await self._transport._call_event_handler("on_client_connected", self._transport.websocket)
        if not self._receive_task:
            self._receive_task = self.create_task(self._receive_messages())
        await self.set_transport_ready(frame)

    async def stop(self, frame: EndFrame) -> None:
        # Keep the receive loop alive: the output side's mark-drained hangup
        # needs the daemon's `mark` echo. Only stop feeding the pipeline.
        self._stopping = True
        await super().stop(frame)

    async def cancel(self, frame: CancelFrame) -> None:
        self._stopping = True
        await super().cancel(frame)
        await self._stop_receiving()

    async def cleanup(self) -> None:
        await super().cleanup()
        await self._stop_receiving()
        await self._transport._socket.close()

    async def _stop_receiving(self) -> None:
        if self._receive_task:
            await self.cancel_task(self._receive_task)
            self._receive_task = None

    async def _receive_messages(self) -> None:
        serializer = self._transport.serializer
        try:
            while (message := await self._transport._socket.receive()) is not None:
                frame = await serializer.deserialize(message)
                if isinstance(frame, InputAudioRawFrame):
                    if not self._stopping:
                        await self.push_audio_frame(frame)
                elif isinstance(frame, SiphonEventFrame):
                    await self._transport._handle_event(frame.event)
                    if self._stopping:
                        continue
                    await self.push_frame(frame)
                    if isinstance(frame.event, Dtmf):
                        dtmf = serializer.dtmf_frame(frame.event)
                        if dtmf is not None:
                            await self.push_frame(dtmf)
        except Exception as e:
            logger.error(
                f"siphon call {self._transport.call_id}: receive loop failed: "
                f"{e.__class__.__name__} ({e})"
            )
        await self._transport._on_socket_gone()


class SiphonOutputTransport(BaseOutputTransport):
    """Pipeline audio and commands out to the daemon, paced at real time.

    Pipecat's stock WebSocket output sleeps half a chunk per chunk (2× real
    time), which a 200 ms playout window cannot absorb; this one sends one
    20 ms frame per 20 ms against a monotonic clock, at most
    ``playout_lead_ms`` early.
    """

    def __init__(self, transport: SiphonTransport, params: SiphonParams, **kwargs: Any) -> None:
        super().__init__(params, **kwargs)
        self._transport = transport
        self._frame_bytes = frame_bytes(transport.start.audio.sample_rate)
        self._lead = params.playout_lead_ms / 1000
        self._prime = params.prime_start_deadline
        self._dtmf_duration_ms = params.dtmf_duration_ms
        self._pending = bytearray()
        self._next_at: float | None = None

    async def start(self, frame: StartFrame) -> None:
        await super().start(frame)
        await self.set_transport_ready(frame)
        if self._prime:
            await self._send_paced(bytes(self._frame_bytes))

    async def stop(self, frame: EndFrame) -> None:
        await super().stop(frame)  # media senders drain queued audio first
        await self._flush_pending()
        await self._transport._finish(drain=True)

    async def cancel(self, frame: CancelFrame) -> None:
        await super().cancel(frame)
        await self._transport._finish(drain=False)

    async def process_frame(self, frame: Frame, direction: FrameDirection) -> None:
        await super().process_frame(frame, direction)
        if isinstance(frame, InterruptionFrame):
            # super() has already cancelled the audio writer, so no stale
            # frame can follow the `clear`.
            self._pending.clear()
            self._next_at = None
            await self._transport._interrupted()

    async def send_message(
        self, frame: OutputTransportMessageFrame | OutputTransportMessageUrgentFrame
    ) -> None:
        payload = await self._transport.serializer.serialize(frame)
        if payload:
            await self._transport._socket.send(payload)

    def _supports_native_dtmf(self) -> bool:
        return True

    async def _write_dtmf_native(self, frame: OutputDTMFFrame | OutputDTMFUrgentFrame) -> None:
        # RFC 2833 via the daemon; it queues digits in order behind audio.
        for button in frame.buttons or []:
            await self._transport.send_command(
                "send_dtmf", digit=button.value, duration_ms=self._dtmf_duration_ms
            )

    async def write_audio_frame(self, frame: OutputAudioRawFrame) -> bool:
        if not self._transport._socket.open:
            return False
        payload = await self._transport.serializer.serialize(frame)
        if not payload:
            return True
        self._pending.extend(payload)
        while len(self._pending) >= self._frame_bytes:
            chunk = bytes(self._pending[: self._frame_bytes])
            del self._pending[: self._frame_bytes]
            if not await self._send_paced(chunk):
                return False
        return True

    async def _flush_pending(self) -> None:
        """Zero-pad and send a trailing partial frame so it isn't lost."""
        if self._pending:
            self._pending.extend(bytes(self._frame_bytes - len(self._pending)))
            chunk = bytes(self._pending)
            self._pending.clear()
            await self._send_paced(chunk)

    async def _send_paced(self, chunk: bytes) -> bool:
        now = asyncio.get_running_loop().time()
        # Idle (or fell behind): re-anchor rather than burst to catch up.
        if self._next_at is None or self._next_at < now:
            self._next_at = now
        wait = self._next_at - self._lead - now
        if wait > 0:
            await asyncio.sleep(wait)
        self._next_at += FRAME_SECS
        return await self._transport._socket.send(chunk)


class SiphonTransport(BaseTransport):
    """A Pipecat transport for one SiphonAI call (one WebSocket, one pipeline).

    Build it with :meth:`accept` from a FastAPI WebSocket endpoint::

        @app.websocket("/ws")
        async def ws(websocket: WebSocket):
            transport = await SiphonTransport.accept(websocket)
            ...
            worker = PipelineWorker(pipeline, params=transport.pipeline_params())

    Event handlers:

    - ``on_client_connected(transport, websocket)`` — pipeline running; a
      good place to queue the greeting.
    - ``on_client_disconnected(transport, websocket)`` — the daemon side
      went away without this transport closing it.
    - ``on_call_stopped(transport, reason)`` — the daemon sent ``stop``
      (§3.9); cancel the pipeline worker here.
    - ``on_siphon_event(transport, event)`` — every protocol event, typed
      (the same events are pushed downstream as :class:`SiphonEventFrame`).
    """

    def __init__(
        self,
        websocket: WebSocket,
        start: Start,
        params: SiphonParams | None = None,
        input_name: str | None = None,
        output_name: str | None = None,
    ) -> None:
        super().__init__(input_name=input_name, output_name=output_name)
        self.websocket = websocket
        self.start = start
        self.call_id = start.call_id
        rate = start.audio.sample_rate
        params = (params or SiphonParams()).model_copy(
            update={"audio_in_sample_rate": rate, "audio_out_sample_rate": rate}
        )
        self._params = params
        self.serializer = SiphonFrameSerializer(start, auto_hang_up=params.auto_hang_up)
        self._socket = _SiphonSocket(websocket)
        self._end_mark = asyncio.Event()
        # Signalled whenever a pending transfer/park may have resolved.
        self._handoff_changed = asyncio.Event()
        self._verdict_task: asyncio.Task | None = None
        self._finished = False
        self._input = SiphonInputTransport(self, params, name=self._input_name)
        self._output = SiphonOutputTransport(self, params, name=self._output_name)

        self._register_event_handler("on_client_connected")
        self._register_event_handler("on_client_disconnected")
        self._register_event_handler("on_call_stopped")
        self._register_event_handler("on_siphon_event")

        mode = start.barge_in_mode
        if mode not in ("notify_only", "pause"):
            reported = mode or "unreported (pre-0.32 daemon)"
            logger.warning(
                f"siphon call {self.call_id}: barge_in_mode is {reported}; "
                "the daemon flushes bot audio on its own VAD, which can contradict Pipecat's "
                'interruption decisions. Set [bridge.barge_in] mode = "notify_only" or "pause" '
                "on routes served by Pipecat."
            )
        logger.info(
            f"siphon call {self.call_id}: {start.from_} -> {start.to} ({start.direction}, "
            f"{rate} Hz, barge_in_mode={mode}{', reconnected' if start.reconnected else ''})"
        )

    @classmethod
    async def accept(
        cls,
        websocket: WebSocket,
        params: SiphonParams | None = None,
        *,
        auth_token: str | None = None,
        start_timeout_secs: float = 10.0,
        **kwargs: Any,
    ) -> SiphonTransport:
        """Complete the WebSocket handshake, read ``start``, and build the
        transport.

        ``auth_token`` must match the daemon's ``[bridge].auth_bearer``
        (``Authorization: Bearer <token>``); a mismatch is refused before
        the upgrade. Raises :class:`SiphonHandshakeError` (socket already
        closed) when the peer isn't a usable protocol v1 session.
        """
        if auth_token is not None:
            presented = websocket.headers.get("authorization", "")
            if not hmac.compare_digest(presented, f"Bearer {auth_token}"):
                await websocket.close(code=1008)
                raise SiphonHandshakeError("bad or missing bearer token")
        offered = websocket.scope.get("subprotocols") or []
        await websocket.accept(subprotocol=SUBPROTOCOL if SUBPROTOCOL in offered else None)

        async def refuse(code: int, why: str) -> SiphonHandshakeError:
            try:
                await websocket.close(code=code, reason=why)
            except Exception:
                pass
            return SiphonHandshakeError(why)

        try:
            message = await asyncio.wait_for(websocket.receive(), timeout=start_timeout_secs)
        except asyncio.TimeoutError:
            raise await refuse(1002, "no start message") from None
        if message["type"] == "websocket.disconnect":
            raise SiphonHandshakeError("closed before start")
        text = message.get("text")
        if text is None:
            raise await refuse(1002, "expected start")
        try:
            event = parse_event(text)
        except ValueError:
            raise await refuse(1002, "expected start") from None
        if not isinstance(event, Start):
            raise await refuse(1002, "expected start")
        if event.version != "1":
            # §5.4: a server unwilling to speak the version closes with 1003.
            raise await refuse(1003, "unsupported version")
        return cls(websocket, event, params, **kwargs)

    def input(self) -> SiphonInputTransport:
        return self._input

    def output(self) -> SiphonOutputTransport:
        return self._output

    def pipeline_params(self, **kwargs: Any) -> PipelineParams:
        """``PipelineParams`` with audio rates matching the call, so TTS
        synthesizes at the wire rate where it can and nothing resamples
        twice. Extra keyword arguments pass through."""
        rate = self.start.audio.sample_rate
        return PipelineParams(audio_in_sample_rate=rate, audio_out_sample_rate=rate, **kwargs)

    # ─── commands (PROTOCOL.md §4) ────────────────────────────────

    async def send_command(self, type: str, **fields: Any) -> bool:
        """Send one protocol v1 command now (out of band). For a command
        ordered behind queued audio, push :func:`siphon_command` instead."""
        return await self._socket.send(self.serializer.command(type, **fields))

    async def hangup(self, cause: str = "normal") -> bool:
        return await self.send_command("hangup", cause=cause)

    async def transfer(
        self, target: str | None = None, *, replaces_call_id: str | None = None
    ) -> bool:
        fields: dict[str, Any] = {}
        if target is not None:
            fields["target"] = target
        if replaces_call_id is not None:
            fields["replaces_call_id"] = replaces_call_id
        return await self.send_command("transfer", **fields)

    async def hold(self) -> bool:
        return await self.send_command("hold")

    async def resume(self) -> bool:
        return await self.send_command("resume")

    async def park(self, slot: str | None = None) -> bool:
        return await self.send_command("park", **({"slot": slot} if slot else {}))

    async def mark(self, name: str) -> bool:
        return await self.send_command("mark", name=name)

    # ─── internals shared by the input/output halves ──────────────

    async def _handle_event(self, event: Event) -> None:
        if isinstance(event, Mark) and event.name == END_MARK:
            self._end_mark.set()
        elif isinstance(event, SpeechStarted) and event.decision_pending:
            self._arm_verdict(event.decision_deadline_ms or 0)
        elif isinstance(event, BargeInResolved):
            self._cancel_verdict()
        elif isinstance(event, Error):
            if event.code in ("transfer_failed", "park_failed"):
                self.serializer.handoff_failed()
                self._handoff_changed.set()
            logger.warning(
                f"siphon call {self.call_id}: daemon error {event.code}: {event.message}"
            )
        elif isinstance(event, Stop):
            self._cancel_verdict()
            self._end_mark.set()
            self._handoff_changed.set()
            logger.info(f"siphon call {self.call_id}: stopped ({event.reason})")
            await self._call_event_handler("on_call_stopped", event.reason)
        await self._call_event_handler("on_siphon_event", event)

    def _arm_verdict(self, deadline_ms: int) -> None:
        """Pause-mode arbitration: Pipecat's interruption is the verdict.
        If none comes before the deadline (less a margin), reject — the
        daemon resumes the bot where it paused."""
        self._cancel_verdict()
        delay = max(0, deadline_ms - self._params.pause_decision_margin_ms) / 1000
        self._verdict_task = asyncio.create_task(self._reject_after(delay))

    async def _reject_after(self, delay: float) -> None:
        await asyncio.sleep(delay)
        self._verdict_task = None
        logger.debug(f"siphon call {self.call_id}: no Pipecat interruption; barge_in_reject")
        await self.send_command("barge_in_reject")

    def _cancel_verdict(self) -> None:
        if self._verdict_task is not None:
            self._verdict_task.cancel()
            self._verdict_task = None

    async def _interrupted(self) -> None:
        # `clear` doubles as `barge_in_confirm` while an arbitration is
        # pending (§4.1), so one message covers every barge-in mode.
        self._cancel_verdict()
        await self._socket.send(self.serializer.command("clear"))

    async def _finish(self, *, drain: bool) -> None:
        """End of pipeline: drain (EndFrame), hang up, close — once."""
        if self._finished:
            return
        self._finished = True
        self._cancel_verdict()
        # A transfer/park in flight ends the call with its own `stop`; a
        # hangup now would BYE the dialog under the REFER. Wait for it to
        # land or fail (then hang up as usual).
        loop = asyncio.get_running_loop()
        deadline = loop.time() + HANDOFF_WAIT_SECS
        while self.serializer.handoff_pending and self._socket.open:
            remaining = deadline - loop.time()
            if remaining <= 0:
                logger.warning(f"siphon call {self.call_id}: transfer/park unresolved; hanging up")
                break
            self._handoff_changed.clear()
            await self._wait(self._handoff_changed, remaining, "transfer/park outcome")
        if self._params.auto_hang_up and not self.serializer.call_ended and self._socket.open:
            if drain and await self.mark(END_MARK):
                await self._wait(self._end_mark, self._params.end_drain_timeout_secs, "end mark")
            hangup = await self.serializer.serialize(EndFrame())
            if hangup:
                await self._socket.send(hangup)
        await self._socket.close()

    async def _wait(self, event: asyncio.Event, timeout: float, what: str) -> None:
        try:
            await asyncio.wait_for(event.wait(), timeout=timeout)
        except asyncio.TimeoutError:
            logger.debug(f"siphon call {self.call_id}: no {what} within {timeout}s")

    async def _on_socket_gone(self) -> None:
        self._cancel_verdict()
        self._end_mark.set()
        self._handoff_changed.set()
        if not self._socket.closed_locally:
            await self._call_event_handler("on_client_disconnected", self.websocket)
