"""Protocol v1 ↔ Pipecat frame mapping. Pure: no timing, no I/O."""

from __future__ import annotations

import json
from typing import Any

from loguru import logger
from pipecat.audio.dtmf.types import KeypadEntry
from pipecat.audio.utils import create_stream_resampler
from pipecat.frames.frames import (
    AudioRawFrame,
    CancelFrame,
    EndFrame,
    Frame,
    InputAudioRawFrame,
    InputDTMFFrame,
    InterruptionFrame,
    OutputTransportMessageFrame,
    OutputTransportMessageUrgentFrame,
)
from pipecat.serializers.base_serializer import FrameSerializer
from siphon_ai_server import Dtmf, Start, Stop, parse_event

from .frames import BRIDGE_IN_COMMANDS, SiphonEventFrame

__all__ = ["SiphonFrameSerializer"]


class SiphonFrameSerializer(FrameSerializer):
    """Maps SiphonAI WebSocket protocol v1 onto Pipecat frames.

    Wire → Pipecat (:meth:`deserialize`):

    - binary 20 ms PCM16-LE frame → ``InputAudioRawFrame`` at the call's rate
    - every JSON event → :class:`SiphonEventFrame` (typed SDK event)

    Pipecat → wire (:meth:`serialize`):

    - ``AudioRawFrame`` → PCM16-LE bytes at the call's rate (resampled only
      if the frame isn't already at it; *not* re-framed — that, and pacing,
      are the transport's job)
    - ``InterruptionFrame`` → ``clear``
    - ``OutputTransportMessage[Urgent]Frame`` carrying a protocol v1
      command (see :func:`siphon_command`) → that command; anything else is
      dropped, since an unknown ``type`` is fatal to the call
    - ``EndFrame`` / ``CancelFrame`` → ``hangup`` (once, and never after the
      daemon's ``stop``) when ``auto_hang_up``

    Built for :class:`~siphon_ai_pipecat.SiphonTransport`; Pipecat's stock
    ``FastAPIWebsocketTransport`` streams at 2× real time, which overruns
    SiphonAI's 200 ms playout window (PROTOCOL.md §5.5).
    """

    def __init__(self, start: Start, *, auto_hang_up: bool = True) -> None:
        super().__init__(
            FrameSerializer.InputParams(
                # Telephony audio is bursty around silence; don't let the
                # resampler drop its history mid-utterance.
                resampler_clear_after_secs=None,
            )
        )
        self._call_id = start.call_id
        self._siphon_rate = start.audio.sample_rate
        self._auto_hang_up = auto_hang_up
        self._hangup_sent = False
        self._call_ended = False
        self._handoff_pending = False
        self._output_resampler = create_stream_resampler(clear_after_secs=None)

    @property
    def call_ended(self) -> bool:
        """True once the daemon sent ``stop`` or a ``hangup`` went out."""
        return self._call_ended or self._hangup_sent

    @property
    def handoff_pending(self) -> bool:
        """A ``transfer`` or ``park`` went out and neither its ``stop`` nor
        its failure has come back. A ``hangup`` now would race it."""
        return self._handoff_pending and not self._call_ended

    def handoff_failed(self) -> None:
        """The daemon refused the handoff; the call continues."""
        self._handoff_pending = False

    def command(self, type: str, **fields: Any) -> str:
        """Encode one protocol v1 command with this call's ``call_id``."""
        if type not in BRIDGE_IN_COMMANDS:
            raise ValueError(f"not a SiphonAI protocol v1 command: {type!r}")
        if type == "hangup":
            self._hangup_sent = True
        elif type in ("transfer", "park"):
            self._handoff_pending = True
        return json.dumps({**fields, "type": type, "call_id": self._call_id})

    # ─── Pipecat → wire ──────────────────────────────────────────

    async def serialize(self, frame: Frame) -> str | bytes | None:
        if isinstance(frame, (EndFrame, CancelFrame)):
            if self._auto_hang_up and not self.call_ended:
                return self.command("hangup", cause="normal")
            return None
        if isinstance(frame, InterruptionFrame):
            return self.command("clear")
        if isinstance(frame, AudioRawFrame):
            pcm = frame.audio
            if frame.sample_rate != self._siphon_rate:
                pcm = await self._output_resampler.resample(
                    pcm, frame.sample_rate, self._siphon_rate
                )
            return pcm or None
        if isinstance(frame, (OutputTransportMessageFrame, OutputTransportMessageUrgentFrame)):
            if self.should_ignore_frame(frame):
                return None
            message = frame.message
            if not isinstance(message, dict) or message.get("type") not in BRIDGE_IN_COMMANDS:
                logger.warning(
                    f"siphon call {self._call_id}: dropping transport message that is "
                    f"not a protocol v1 command: {message!r}"
                )
                return None
            fields = {k: v for k, v in message.items() if k not in ("type", "call_id", "seq")}
            return self.command(message["type"], **fields)
        return None

    # ─── wire → Pipecat ──────────────────────────────────────────

    async def deserialize(self, data: str | bytes) -> Frame | None:
        if isinstance(data, (bytes, bytearray)):
            # Always the call's rate: the transport pins its input rate to
            # it, and STT services resample their own input.
            return InputAudioRawFrame(
                audio=bytes(data), sample_rate=self._siphon_rate, num_channels=1
            )
        try:
            event = parse_event(data)
        except ValueError as e:
            # The daemon never sends malformed JSON; keep the call alive if
            # something on the path does.
            logger.warning(f"siphon call {self._call_id}: ignoring bad text frame: {e}")
            return None
        if isinstance(event, Stop):
            self._call_ended = True
        return SiphonEventFrame(event=event)

    @staticmethod
    def dtmf_frame(event: Dtmf) -> InputDTMFFrame | None:
        """Pipecat's native DTMF frame for a ``dtmf`` event, or ``None`` for
        the A–D keys, which Pipecat's ``KeypadEntry`` doesn't model (they
        still arrive as a :class:`SiphonEventFrame`)."""
        try:
            return InputDTMFFrame(button=KeypadEntry(event.digit))
        except ValueError:
            return None
