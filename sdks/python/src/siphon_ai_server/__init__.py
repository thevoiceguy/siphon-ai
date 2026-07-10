"""Server SDK for the SiphonAI WebSocket bridge protocol v1.

Write handlers, not wire code:

```python
import asyncio
from siphon_ai_server import AudioFrame, Dtmf, SiphonServer


async def handle(call):
    print("call from", call.start.from_)
    async for item in call:
        if isinstance(item, AudioFrame):
            await call.send_audio_frame(item.pcm)   # echo
        elif isinstance(item, Dtmf) and item.digit == "0":
            await call.hangup()


asyncio.run(SiphonServer(handle, port=8080).serve_forever())
```

The canonical protocol spec is ``docs/PROTOCOL.md``; the machine-readable
contract is ``schemas/siphon-ai.v1.json`` (this SDK's test suite validates
against both). **This SDK contains no AI code** — STT/LLM/TTS are your
handler's business.
"""

from .audio import FRAME_MS, AudioSender, frame_bytes
from .call import Call
from .events import (
    AudioFormat,
    AudioFrame,
    ConferenceJoined,
    ConferenceLeft,
    DeadAirDetected,
    Dtmf,
    Error,
    Event,
    FarEndHold,
    FarEndResume,
    Held,
    Mark,
    ParticipantJoined,
    ParticipantLeft,
    RecordingFailed,
    RecordingStarted,
    RecordingStopped,
    Resumed,
    RtpStats,
    SilenceDetected,
    SipMeta,
    SpeechStarted,
    SpeechStopped,
    Start,
    Stop,
    TraceContext,
    UnknownEvent,
    parse_event,
)
from .server import SUBPROTOCOL, SiphonServer

__version__ = "0.28.0"

__all__ = [
    "AudioFormat",
    "AudioFrame",
    "AudioSender",
    "Call",
    "ConferenceJoined",
    "ConferenceLeft",
    "DeadAirDetected",
    "Dtmf",
    "Error",
    "Event",
    "FarEndHold",
    "FarEndResume",
    "FRAME_MS",
    "frame_bytes",
    "Held",
    "Mark",
    "ParticipantJoined",
    "ParticipantLeft",
    "parse_event",
    "RecordingFailed",
    "RecordingStarted",
    "RecordingStopped",
    "Resumed",
    "RtpStats",
    "SilenceDetected",
    "SipMeta",
    "SpeechStarted",
    "SpeechStopped",
    "Start",
    "Stop",
    "SUBPROTOCOL",
    "SiphonServer",
    "TraceContext",
    "UnknownEvent",
]
