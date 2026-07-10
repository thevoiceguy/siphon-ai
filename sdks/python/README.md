# siphon-ai-server (Python)

Server SDK for the [SiphonAI](https://github.com/thevoiceguy/siphon-ai)
WebSocket bridge protocol v1: typed events, paced 20 ms audio framing, and
connection lifecycle — write handlers, not wire code.

**This SDK contains no AI code.** SiphonAI streams call audio to your server
and plays back whatever you send; STT/LLM/TTS are your handler's business.

## Install

Not yet on PyPI — install from the repo (vendorable):

```bash
pip install ./sdks/python
# or from a checkout URL:
pip install "siphon-ai-server @ git+https://github.com/thevoiceguy/siphon-ai#subdirectory=sdks/python"
```

Requires Python ≥ 3.10. Sole runtime dependency: `websockets`.

## Quickstart

```python
import asyncio
from siphon_ai_server import AudioFrame, Dtmf, SiphonServer


async def handle(call):
    print("call from", call.start.from_, "to", call.start.to)
    async for item in call:
        if isinstance(item, AudioFrame):
            await call.send_audio_frame(item.pcm)   # echo it back
        elif isinstance(item, Dtmf) and item.digit == "0":
            await call.hangup()


asyncio.run(SiphonServer(handle, port=8080).serve_forever())
```

Point a SiphonAI route at `ws://your-host:8080/` and place a call.

## What you get

- **`SiphonServer(handler, host, port)`** — accepts WS connections, echoes
  the `siphon-ai.v1` subprotocol, waits for `start`, and invokes your
  handler with a `Call`. One task per call; handler exceptions tear down
  only that call.
- **`Call`** — async-iterate it to receive `AudioFrame` (binary PCM) and
  typed events (`Dtmf`, `SpeechStarted`, `Mark`, `RtpStats`, `Stop`, …).
  Iteration ends when the call does.
- **Commands** — one method per protocol v1 command: `send_audio` /
  `send_audio_frame`, `clear`, `mark`, `hangup`, `transfer`, `send_dtmf`,
  `mute`/`unmute`, `start_recording`/`stop_recording`/`pause_recording`/
  `resume_recording`, `set_recording_consent`, `park`, `hold`/`resume`,
  `conference_join`/`conference_leave`, `abort`.
- **`send_audio(pcm)`** takes arbitrary byte lengths and re-frames to exact
  20 ms frames (320 B @ 8 kHz, 640 B @ 16 kHz — per `call.start.audio`),
  paced at 50 frames/s. `send_audio_frame` sends one pre-framed frame.
- **Close semantics** per PROTOCOL §5.7: end a call with `hangup()`; a bare
  close is treated as a drop (and triggers daemon-side reconnect if
  enabled). `call.start.reconnected` tells you a session is a resumption.
- **Tolerant parsing** — unknown event types arrive as `UnknownEvent`,
  unknown fields are ignored, so older SDKs keep working as the daemon
  grows.

## Conformance

The test suite parses every canonical example in `docs/PROTOCOL.md` into
typed events and validates every command the SDK can emit against
`schemas/siphon-ai.v1.json`:

```bash
pip install ./sdks/python[test] pytest
pytest sdks/python/tests
```

The canonical spec is [`docs/PROTOCOL.md`](../../docs/PROTOCOL.md); the
machine-readable contract is
[`schemas/siphon-ai.v1.json`](../../schemas/siphon-ai.v1.json).

A complete working server built on this SDK:
[`examples/echo-ws-server-python`](../../examples/echo-ws-server-python).
