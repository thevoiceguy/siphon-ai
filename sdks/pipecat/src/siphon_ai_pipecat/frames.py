"""Pipecat frames for SiphonAI protocol events and commands."""

from __future__ import annotations

from dataclasses import dataclass
from typing import Any

from pipecat.frames.frames import (
    OutputTransportMessageFrame,
    OutputTransportMessageUrgentFrame,
    SystemFrame,
)
from siphon_ai_server import Event

__all__ = ["BRIDGE_IN_COMMANDS", "SiphonEventFrame", "siphon_command"]

# Every server→SiphonAI `type` in protocol v1 (PROTOCOL.md §4). The daemon
# answers any other `type` with a *fatal* `protocol_error`, so the
# serializer refuses to forward anything outside this set. The test suite
# pins it to `schemas/siphon-ai.v1.json` `$defs/BridgeIn`.
BRIDGE_IN_COMMANDS = frozenset(
    {
        "clear",
        "mark",
        "hangup",
        "transfer",
        "send_dtmf",
        "mute",
        "unmute",
        "start_recording",
        "stop_recording",
        "pause_recording",
        "resume_recording",
        "set_recording_consent",
        "conference_join",
        "conference_leave",
        "park",
        "hold",
        "resume",
        "barge_in_confirm",
        "barge_in_reject",
    }
)


@dataclass
class SiphonEventFrame(SystemFrame):
    """A SiphonAI→server protocol event (PROTOCOL.md §3), typed by the
    ``siphon-ai-server`` SDK — ``SpeechStarted``, ``FarEndHold``,
    ``RtpStats``, ``UnknownEvent``, …

    Pushed downstream by :class:`~siphon_ai_pipecat.SiphonTransport` so
    custom processors can react to call events in-pipeline. Processors that
    don't know it pass it through untouched.
    """

    event: Event

    def __str__(self) -> str:
        return f"{self.name}({self.event.type})"


def siphon_command(
    type: str, /, *, urgent: bool = False, **fields: Any
) -> OutputTransportMessageFrame | OutputTransportMessageUrgentFrame:
    """Build a frame that sends one SiphonAI command (PROTOCOL.md §4).

    The default (non-urgent) frame is a ``DataFrame``: the transport sends
    it **after the audio queued before it**, so pushing TTS audio and then
    ``siphon_command("transfer", target="sip:agent@pbx")`` transfers once
    the goodbye has been handed to the daemon. ``urgent=True`` sends
    immediately. ``call_id`` is filled in by the serializer.

    Raises ``ValueError`` for a ``type`` that isn't a protocol v1 command —
    the daemon would tear the call down with ``protocol_error``.
    """
    if type not in BRIDGE_IN_COMMANDS:
        raise ValueError(f"not a SiphonAI protocol v1 command: {type!r}")
    message = {"type": type, **fields}
    if urgent:
        return OutputTransportMessageUrgentFrame(message=message)
    return OutputTransportMessageFrame(message=message)
