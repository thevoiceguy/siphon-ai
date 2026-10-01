"""SiphonFrameSerializer: protocol v1 ↔ Pipecat frame mapping (no I/O).

Every command the serializer can emit is validated against
``schemas/siphon-ai.v1.json`` ``$defs/BridgeIn`` — an unknown or malformed
command is *fatal* to a live call (``protocol_error``).
"""

from __future__ import annotations

import asyncio
import json
from pathlib import Path

import jsonschema
import pytest
from pipecat.frames.frames import (
    CancelFrame,
    EndFrame,
    InputAudioRawFrame,
    InterruptionFrame,
    OutputAudioRawFrame,
    OutputTransportMessageFrame,
    OutputTransportMessageUrgentFrame,
)
from siphon_ai_pipecat import (
    BRIDGE_IN_COMMANDS,
    SiphonEventFrame,
    SiphonFrameSerializer,
    siphon_command,
)
from siphon_ai_server import Dtmf, SpeechStarted, UnknownEvent, parse_event

REPO = Path(__file__).resolve().parents[3]
SCHEMA = json.loads((REPO / "schemas" / "siphon-ai.v1.json").read_text())
BRIDGE_IN = jsonschema.Draft202012Validator({"$ref": "#/$defs/BridgeIn", "$defs": SCHEMA["$defs"]})

CALL_ID = "siphon-test"

# One schema-valid instance of every command, fields only.
COMMAND_SAMPLES: dict[str, dict] = {
    "clear": {},
    "mark": {"name": "m1"},
    "hangup": {"cause": "normal"},
    "transfer": {"target": "sip:agent@pbx.example.com"},
    "send_dtmf": {"digit": "5", "duration_ms": 160},
    "mute": {},
    "unmute": {},
    "start_recording": {},
    "stop_recording": {},
    "pause_recording": {},
    "resume_recording": {},
    "set_recording_consent": {"note": "dtmf-1"},
    "conference_join": {"room_id": "room-1"},
    "conference_leave": {},
    "park": {"slot": "lot-3"},
    "hold": {},
    "resume": {},
    "barge_in_confirm": {},
    "barge_in_reject": {},
}


def start_msg(rate: int = 8000, **extra) -> str:
    return json.dumps(
        {
            "type": "start",
            "version": "1",
            "call_id": CALL_ID,
            "seq": 0,
            "from": "+13125551212",
            "to": "5000",
            "direction": "inbound",
            "audio": {"encoding": "pcm16le", "sample_rate": rate, "channels": 1, "frame_ms": 20},
            "sip": {"call_id": "abc@pbx", "headers": {}},
            **extra,
        }
    )


def make(rate: int = 8000, **kw) -> SiphonFrameSerializer:
    return SiphonFrameSerializer(parse_event(start_msg(rate)), **kw)


def run(coro):
    return asyncio.run(coro)


def schema_commands() -> set[str]:
    return {v["properties"]["type"]["const"] for v in SCHEMA["$defs"]["BridgeIn"]["oneOf"]}


def test_command_set_matches_schema():
    assert BRIDGE_IN_COMMANDS == schema_commands()
    assert set(COMMAND_SAMPLES) == schema_commands()


@pytest.mark.parametrize("type_", sorted(COMMAND_SAMPLES))
def test_every_command_is_schema_valid(type_):
    s = make()
    wire = json.loads(s.command(type_, **COMMAND_SAMPLES[type_]))
    assert wire["call_id"] == CALL_ID
    assert "seq" not in wire
    BRIDGE_IN.validate(wire)


@pytest.mark.parametrize("type_", sorted(COMMAND_SAMPLES))
def test_siphon_command_frames_round_trip(type_):
    s = make()
    for urgent in (False, True):
        frame = siphon_command(type_, urgent=urgent, **COMMAND_SAMPLES[type_])
        assert isinstance(
            frame, OutputTransportMessageUrgentFrame if urgent else OutputTransportMessageFrame
        )
        wire = json.loads(run(s.serialize(frame)))
        BRIDGE_IN.validate(wire)
        assert wire["type"] == type_


def test_unknown_command_refused():
    with pytest.raises(ValueError):
        siphon_command("answer")
    with pytest.raises(ValueError):
        make().command("answer")


def test_foreign_transport_messages_dropped():
    s = make()
    # Not a protocol command: would be a fatal protocol_error on the wire.
    assert run(s.serialize(OutputTransportMessageFrame(message={"type": "bogus"}))) is None
    assert run(s.serialize(OutputTransportMessageFrame(message={"event": "media"}))) is None
    # RTVI chatter is never forwarded.
    rtvi = OutputTransportMessageUrgentFrame(message={"label": "rtvi-ai", "type": "clear"})
    assert run(s.serialize(rtvi)) is None


def test_caller_supplied_call_id_and_seq_are_overridden():
    s = make()
    frame = OutputTransportMessageFrame(message={"type": "hold", "call_id": "other", "seq": 9})
    wire = json.loads(run(s.serialize(frame)))
    assert wire == {"type": "hold", "call_id": CALL_ID}


def test_interruption_is_clear():
    wire = json.loads(run(make().serialize(InterruptionFrame())))
    assert wire == {"type": "clear", "call_id": CALL_ID}


def test_audio_at_call_rate_passes_through():
    pcm = bytes(range(256)) * 2 + bytes(128)  # 640 B
    frame = OutputAudioRawFrame(audio=pcm, sample_rate=16000, num_channels=1)
    assert run(make(16000).serialize(frame)) == pcm


def test_audio_at_other_rate_is_resampled():
    async def go():
        s = make(8000)
        total = 0
        for _ in range(10):  # 10 × 20 ms @ 24 kHz
            out = await s.serialize(
                OutputAudioRawFrame(audio=bytes(960), sample_rate=24000, num_channels=1)
            )
            total += len(out or b"")
        return total

    # 200 ms @ 8 kHz = 3200 B; the streaming resampler holds back ~60 ms of
    # filter history. A fallback path only: the transport pins the output
    # rate to the call's, so Pipecat's own resampler (which flushes on
    # TTSStoppedFrame) does the real conversion.
    assert 2000 <= run(go()) <= 3200


def test_hangup_on_end_once():
    s = make()
    wire = json.loads(run(s.serialize(EndFrame())))
    assert wire == {"type": "hangup", "call_id": CALL_ID, "cause": "normal"}
    assert s.call_ended
    assert run(s.serialize(CancelFrame())) is None


def test_no_hangup_after_stop():
    s = make()
    run(
        s.deserialize(
            json.dumps({"type": "stop", "call_id": CALL_ID, "seq": 5, "reason": "caller_hangup"})
        )
    )
    assert s.call_ended
    assert run(s.serialize(EndFrame())) is None


def test_auto_hang_up_off():
    assert run(make(auto_hang_up=False).serialize(CancelFrame())) is None


def test_inbound_audio():
    frame = run(make(16000).deserialize(bytes(640)))
    assert isinstance(frame, InputAudioRawFrame)
    assert frame.sample_rate == 16000 and frame.num_channels == 1 and len(frame.audio) == 640


def test_inbound_events_are_typed():
    s = make()
    dtmf = run(
        s.deserialize(
            json.dumps(
                {
                    "type": "dtmf",
                    "call_id": CALL_ID,
                    "seq": 3,
                    "digit": "5",
                    "duration_ms": 120,
                    "method": "rfc2833",
                }
            )
        )
    )
    assert isinstance(dtmf, SiphonEventFrame) and isinstance(dtmf.event, Dtmf)
    assert s.dtmf_frame(dtmf.event).button.value == "5"

    a_key = Dtmf(call_id=CALL_ID, seq=4, digit="A", duration_ms=100, method="rfc2833")
    assert s.dtmf_frame(a_key) is None  # Pipecat's KeypadEntry has no A–D

    speech = run(
        s.deserialize(
            json.dumps(
                {
                    "type": "speech_started",
                    "call_id": CALL_ID,
                    "seq": 6,
                    "ts_ms": 1,
                    "bot_playing": True,
                }
            )
        )
    )
    assert isinstance(speech.event, SpeechStarted)


def test_unknown_and_malformed_events_never_raise():
    s = make()
    future = run(
        s.deserialize(json.dumps({"type": "from_the_future", "call_id": CALL_ID, "seq": 1}))
    )
    assert isinstance(future.event, UnknownEvent)
    assert run(s.deserialize("{not json")) is None
