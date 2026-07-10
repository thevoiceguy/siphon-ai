"""Typed events for the SiphonAI WS protocol v1 (daemon → server).

One frozen dataclass per `BridgeOut` message, plus :class:`AudioFrame` for
the binary half. Shapes mirror ``schemas/siphon-ai.v1.json`` exactly — the
SDK test suite validates this module against the schema and every example
in ``docs/PROTOCOL.md``.

Parsing is **tolerant by contract**: unknown JSON fields are ignored and an
unknown ``type`` becomes :class:`UnknownEvent` (the protocol is additive
within v1 — a server must keep working when the daemon learns new tricks).
"""

from __future__ import annotations

import json
from dataclasses import dataclass, field, fields
from typing import Any, Mapping, Union

__all__ = [
    "AudioFrame",
    "AudioFormat",
    "SipMeta",
    "TraceContext",
    "Start",
    "SpeechStarted",
    "SpeechStopped",
    "FarEndHold",
    "FarEndResume",
    "SilenceDetected",
    "DeadAirDetected",
    "RtpStats",
    "Dtmf",
    "Mark",
    "RecordingStarted",
    "RecordingStopped",
    "RecordingFailed",
    "ConferenceJoined",
    "ConferenceLeft",
    "ParticipantJoined",
    "ParticipantLeft",
    "Held",
    "Resumed",
    "Stop",
    "Error",
    "UnknownEvent",
    "Event",
    "parse_event",
]


@dataclass(frozen=True)
class AudioFrame:
    """One WS binary frame: raw PCM16-LE mono, exactly 20 ms of audio.

    Not a JSON message — yielded by :class:`~siphon_ai_server.Call`'s
    iterator alongside the typed events below.
    """

    pcm: bytes

    @property
    def sample_count(self) -> int:
        return len(self.pcm) // 2


# ─── nested objects ──────────────────────────────────────────────────────


@dataclass(frozen=True)
class AudioFormat:
    encoding: str
    sample_rate: int
    channels: int
    frame_ms: int


@dataclass(frozen=True)
class SipMeta:
    call_id: str
    headers: Mapping[str, str] = field(default_factory=dict)


@dataclass(frozen=True)
class TraceContext:
    traceparent: str
    tracestate: str | None = None


# ─── events (BridgeOut) ──────────────────────────────────────────────────


@dataclass(frozen=True)
class Start:
    """First message of every session. ``from_`` is the wire's ``from``."""

    type = "start"
    version: str
    call_id: str
    seq: int
    from_: str
    to: str
    direction: str
    audio: AudioFormat
    sip: SipMeta
    srtp: Mapping[str, Any] | None = None
    verstat: Mapping[str, Any] | None = None
    trace_context: TraceContext | None = None
    retrieved: bool = False
    reconnected: bool = False


@dataclass(frozen=True)
class SpeechStarted:
    type = "speech_started"
    call_id: str
    seq: int
    ts_ms: int


@dataclass(frozen=True)
class SpeechStopped:
    type = "speech_stopped"
    call_id: str
    seq: int
    ts_ms: int
    duration_ms: int


@dataclass(frozen=True)
class FarEndHold:
    """Wire ``type: hold`` (daemon→server): the far end put the call on
    hold. Distinct from the ``hold`` *command* the server sends."""

    type = "hold"
    call_id: str
    seq: int
    direction: str


@dataclass(frozen=True)
class FarEndResume:
    """Wire ``type: resume`` (daemon→server)."""

    type = "resume"
    call_id: str
    seq: int


@dataclass(frozen=True)
class SilenceDetected:
    type = "silence_detected"
    call_id: str
    seq: int
    duration_ms: int


@dataclass(frozen=True)
class DeadAirDetected:
    type = "dead_air_detected"
    call_id: str
    seq: int
    duration_ms: int


@dataclass(frozen=True)
class RtpStats:
    type = "rtp_stats"
    call_id: str
    seq: int
    jitter_ms: float | None = None
    packet_loss_ratio: float | None = None
    rtcp_rtt_ms: float | None = None


@dataclass(frozen=True)
class Dtmf:
    type = "dtmf"
    call_id: str
    seq: int
    digit: str
    duration_ms: int
    method: str


@dataclass(frozen=True)
class Mark:
    """Playout-position echo of a ``mark`` the server sent."""

    type = "mark"
    call_id: str
    seq: int
    name: str


@dataclass(frozen=True)
class RecordingStarted:
    type = "recording_started"
    call_id: str
    seq: int
    recording_id: str


@dataclass(frozen=True)
class RecordingStopped:
    type = "recording_stopped"
    call_id: str
    seq: int
    recording_id: str


@dataclass(frozen=True)
class RecordingFailed:
    type = "recording_failed"
    call_id: str
    seq: int
    recording_id: str
    reason: str


@dataclass(frozen=True)
class ConferenceJoined:
    type = "conference_joined"
    call_id: str
    seq: int
    room_id: str
    participants: int


@dataclass(frozen=True)
class ConferenceLeft:
    type = "conference_left"
    call_id: str
    seq: int
    room_id: str
    reason: str


@dataclass(frozen=True)
class ParticipantJoined:
    type = "participant_joined"
    call_id: str
    seq: int
    room_id: str
    participant_call_id: str


@dataclass(frozen=True)
class ParticipantLeft:
    type = "participant_left"
    call_id: str
    seq: int
    room_id: str
    participant_call_id: str


@dataclass(frozen=True)
class Held:
    """Ack: the bot-requested hold is active."""

    type = "held"
    call_id: str
    seq: int


@dataclass(frozen=True)
class Resumed:
    """Ack: the bot-requested hold ended."""

    type = "resumed"
    call_id: str
    seq: int


@dataclass(frozen=True)
class Stop:
    """Last message of a session; the daemon closes after sending it."""

    type = "stop"
    call_id: str
    seq: int
    reason: str


@dataclass(frozen=True)
class Error:
    type = "error"
    call_id: str
    seq: int
    code: str
    message: str


@dataclass(frozen=True)
class UnknownEvent:
    """A ``type`` this SDK version doesn't know. The protocol is additive
    within v1, so carry the raw payload rather than failing the call."""

    type: str
    raw: Mapping[str, Any]


Event = Union[
    Start,
    SpeechStarted,
    SpeechStopped,
    FarEndHold,
    FarEndResume,
    SilenceDetected,
    DeadAirDetected,
    RtpStats,
    Dtmf,
    Mark,
    RecordingStarted,
    RecordingStopped,
    RecordingFailed,
    ConferenceJoined,
    ConferenceLeft,
    ParticipantJoined,
    ParticipantLeft,
    Held,
    Resumed,
    Stop,
    Error,
    UnknownEvent,
]

_EVENT_TYPES: dict[str, type] = {
    "start": Start,
    "speech_started": SpeechStarted,
    "speech_stopped": SpeechStopped,
    "hold": FarEndHold,
    "resume": FarEndResume,
    "silence_detected": SilenceDetected,
    "dead_air_detected": DeadAirDetected,
    "rtp_stats": RtpStats,
    "dtmf": Dtmf,
    "mark": Mark,
    "recording_started": RecordingStarted,
    "recording_stopped": RecordingStopped,
    "recording_failed": RecordingFailed,
    "conference_joined": ConferenceJoined,
    "conference_left": ConferenceLeft,
    "participant_joined": ParticipantJoined,
    "participant_left": ParticipantLeft,
    "held": Held,
    "resumed": Resumed,
    "stop": Stop,
    "error": Error,
}

# JSON key → dataclass field renames (Python keywords).
_RENAMES = {"from": "from_"}


def parse_event(text: str | bytes | Mapping[str, Any]) -> Event:
    """Parse one JSON text frame into a typed event.

    Raises ``ValueError`` for malformed JSON or a payload missing required
    fields; returns :class:`UnknownEvent` for a well-formed message whose
    ``type`` this SDK doesn't know.
    """
    if isinstance(text, (str, bytes)):
        try:
            payload = json.loads(text)
        except json.JSONDecodeError as e:
            raise ValueError(f"malformed protocol JSON: {e}") from e
    else:
        payload = dict(text)
    if not isinstance(payload, Mapping) or not isinstance(payload.get("type"), str):
        raise ValueError("protocol message must be an object with a string `type`")

    cls = _EVENT_TYPES.get(payload["type"])
    if cls is None:
        return UnknownEvent(type=payload["type"], raw=payload)

    known = {f.name for f in fields(cls)}
    kwargs: dict[str, Any] = {}
    for key, value in payload.items():
        name = _RENAMES.get(key, key)
        if name == "type" or name not in known:
            continue  # tolerant: additive fields are expected within v1
        kwargs[name] = value

    if cls is Start:
        kwargs["audio"] = _load(AudioFormat, kwargs.get("audio"), "audio")
        kwargs["sip"] = _load(SipMeta, kwargs.get("sip"), "sip")
        if kwargs.get("trace_context") is not None:
            kwargs["trace_context"] = _load(
                TraceContext, kwargs["trace_context"], "trace_context"
            )
    try:
        return cls(**kwargs)
    except TypeError as e:
        raise ValueError(f"invalid `{payload['type']}` message: {e}") from e


def _load(cls: type, value: Any, where: str) -> Any:
    if not isinstance(value, Mapping):
        raise ValueError(f"invalid `{where}` object")
    known = {f.name for f in fields(cls)}
    try:
        return cls(**{k: v for k, v in value.items() if k in known})
    except TypeError as e:
        raise ValueError(f"invalid `{where}` object: {e}") from e
