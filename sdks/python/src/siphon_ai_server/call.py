"""One bridged call: typed event stream + command senders."""

from __future__ import annotations

import json
import logging
from typing import Any, AsyncIterator, Union

from websockets.asyncio.server import ServerConnection
from websockets.exceptions import ConnectionClosed

from .audio import AudioSender
from .events import AudioFrame, Event, Start, parse_event

__all__ = ["Call"]

logger = logging.getLogger("siphon_ai_server")


class Call:
    """A live SiphonAI bridge session.

    Iterate it to receive :class:`AudioFrame` (caller audio, one 20 ms
    frame each) interleaved with typed protocol events. The iterator ends
    when the daemon closes the session (normally right after a ``stop``
    event).

    Command methods mirror `docs/PROTOCOL.md` §4. Ending a call is
    :meth:`hangup` — per §5.7 a bare WS close is treated as an unexpected
    drop, not a hangup.
    """

    def __init__(self, ws: ServerConnection, start: Start) -> None:
        self._ws = ws
        self.start = start
        self.call_id = start.call_id
        self.audio_out = AudioSender(ws.send, start.audio.sample_rate)

    # ─── receiving ────────────────────────────────────────────────

    def __aiter__(self) -> AsyncIterator[Union[AudioFrame, Event]]:
        return self._events()

    async def _events(self) -> AsyncIterator[Union[AudioFrame, Event]]:
        try:
            async for message in self._ws:
                if isinstance(message, bytes):
                    yield AudioFrame(pcm=message)
                else:
                    try:
                        yield parse_event(message)
                    except ValueError as e:
                        # The daemon never sends malformed JSON; if
                        # something on the path does, a robust server
                        # logs and keeps the call alive.
                        logger.warning(
                            "call %s: ignoring bad text frame: %s",
                            self.call_id,
                            e,
                        )
        except ConnectionClosed:
            return
        finally:
            await self.audio_out.aclose()

    # ─── audio ────────────────────────────────────────────────────

    def send_audio(self, pcm: bytes) -> None:
        """Queue PCM16-LE mono bytes (any length) — re-framed to exact
        20 ms frames and paced at real time."""
        self.audio_out.push(pcm)

    async def send_audio_frame(self, frame: bytes) -> None:
        """Send one already-exact 20 ms frame immediately (no pacing) —
        for echo/replay servers that mirror the daemon's own cadence."""
        await self._ws.send(frame)

    async def clear(self) -> None:
        """Barge-in: drop locally buffered audio AND tell the daemon to
        flush everything already queued on its side."""
        self.audio_out.clear()
        await self._command({"type": "clear"})

    # ─── commands (PROTOCOL.md §4) ────────────────────────────────

    async def mark(self, name: str) -> None:
        await self._command({"type": "mark", "name": name})

    async def hangup(self, cause: str = "normal") -> None:
        await self._command({"type": "hangup", "cause": cause})

    async def transfer(
        self,
        target: str | None = None,
        *,
        replaces_call_id: str | None = None,
    ) -> None:
        msg: dict[str, Any] = {"type": "transfer"}
        if target is not None:
            msg["target"] = target
        if replaces_call_id is not None:
            msg["replaces_call_id"] = replaces_call_id
        await self._command(msg)

    async def send_dtmf(self, digit: str, duration_ms: int = 160) -> None:
        await self._command(
            {"type": "send_dtmf", "digit": digit, "duration_ms": duration_ms}
        )

    async def barge_in_confirm(self) -> None:
        """Verdict on a pending pause-mode barge-in arbitration (0.32.0):
        the speech was a real interruption — the daemon drops the retained
        playout tail. A no-op when no arbitration is pending."""
        await self._command({"type": "barge_in_confirm"})

    async def barge_in_reject(self) -> None:
        """Verdict on a pending pause-mode barge-in arbitration (0.32.0):
        false positive — playout resumes where it stopped. A no-op when no
        arbitration is pending."""
        await self._command({"type": "barge_in_reject"})

    async def mute(self) -> None:
        await self._command({"type": "mute"})

    async def unmute(self) -> None:
        await self._command({"type": "unmute"})

    async def start_recording(self) -> None:
        await self._command({"type": "start_recording"})

    async def stop_recording(self) -> None:
        await self._command({"type": "stop_recording"})

    async def pause_recording(self) -> None:
        await self._command({"type": "pause_recording"})

    async def resume_recording(self) -> None:
        await self._command({"type": "resume_recording"})

    async def set_recording_consent(self, note: str | None = None) -> None:
        msg: dict[str, Any] = {"type": "set_recording_consent"}
        if note is not None:
            msg["note"] = note
        await self._command(msg)

    async def park(self, slot: str | None = None) -> None:
        msg: dict[str, Any] = {"type": "park"}
        if slot is not None:
            msg["slot"] = slot
        await self._command(msg)

    async def conference_join(self, room_id: str) -> None:
        await self._command({"type": "conference_join", "room_id": room_id})

    async def conference_leave(self) -> None:
        await self._command({"type": "conference_leave"})

    async def hold(self) -> None:
        await self._command({"type": "hold"})

    async def resume(self) -> None:
        await self._command({"type": "resume"})

    # ─── lifecycle ────────────────────────────────────────────────

    async def abort(self) -> None:
        """Hard-drop the socket without a hangup — the daemon treats this
        as an unexpected WS drop (reconnect/teardown per its config).
        Test harnesses use this; bots want :meth:`hangup`."""
        await self.audio_out.aclose()
        await self._ws.close(code=1011, reason="aborted")

    async def _command(self, msg: dict[str, Any]) -> None:
        msg.setdefault("call_id", self.call_id)
        try:
            await self._ws.send(json.dumps(msg))
        except ConnectionClosed:
            # Commands racing teardown are best-effort by contract.
            pass
