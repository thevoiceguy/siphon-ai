# siphon-ai-server (TypeScript)

Server SDK for the [SiphonAI](https://github.com/thevoiceguy/siphon-ai)
WebSocket bridge protocol v1: typed events (discriminated unions), paced
20 ms audio framing, and connection lifecycle — write handlers, not wire
code.

**This SDK contains no AI code.** SiphonAI streams call audio to your server
and plays back whatever you send; STT/LLM/TTS are your handler's business.

## Install

Not yet on npm — install from the repo (vendorable):

```bash
# One-time in the checkout: install + build the SDK in place (npm links
# local folder deps rather than packing them, so `dist/` must exist).
(cd sdks/typescript && npm install)

npm install ./sdks/typescript
```

Requires Node ≥ 20. Sole runtime dependency: `ws`.

## Quickstart

```ts
import { SiphonServer } from "siphon-ai-server";

const server = new SiphonServer(async (call) => {
  console.log("call from", call.start.from, "to", call.start.to);
  for await (const item of call) {
    if (item.type === "audio") call.sendAudioFrame(item.pcm); // echo
    else if (item.type === "dtmf" && item.digit === "0") call.hangup();
  }
});
await server.listen({ port: 8080 });
```

Point a SiphonAI route at `ws://your-host:8080/` and place a call.

## What you get

- **`SiphonServer(handler)`** — accepts WS connections, echoes the
  `siphon-ai.v1` subprotocol, waits for `start`, and invokes your handler
  with a `Call`. Handler exceptions tear down only that call.
- **`Call`** — `for await` it to receive `{ type: "audio", pcm }` frames
  and typed `BridgeEvent`s (`dtmf`, `speech_started`, `mark`, `rtp_stats`,
  `stop`, …) as a discriminated union on `type`. Iteration ends when the
  call does.
- **Commands** — one method per protocol v1 command: `sendAudio` /
  `sendAudioFrame`, `clear`, `mark`, `hangup`, `transfer`, `sendDtmf`,
  `mute`/`unmute`, `startRecording`/`stopRecording`/`pauseRecording`/
  `resumeRecording`, `setRecordingConsent`, `park`, `hold`/`resume`,
  `conferenceJoin`/`conferenceLeave`, `abort`.
- **`sendAudio(pcm)`** takes arbitrary byte lengths and re-frames to exact
  20 ms frames (320 B @ 8 kHz, 640 B @ 16 kHz — per `call.start.audio`),
  paced at 50 frames/s. `sendAudioFrame` sends one pre-framed frame.
- **Close semantics** per PROTOCOL §5.7: end a call with `hangup()`; a bare
  close is treated as a drop (and triggers daemon-side reconnect if
  enabled). `call.start.reconnected` tells you a session is a resumption.
- **Tolerant parsing** — unknown event types arrive as
  `{ type: "unknown", raw }`, unknown fields are ignored, so older SDKs
  keep working as the daemon grows.

## Conformance

The test suite parses every canonical example in `docs/PROTOCOL.md` into
typed events and validates every command the SDK can emit against
`schemas/siphon-ai.v1.json` (via `ajv`):

```bash
cd sdks/typescript && npm install && npm test
```

The canonical spec is [`docs/PROTOCOL.md`](../../docs/PROTOCOL.md); the
machine-readable contract is
[`schemas/siphon-ai.v1.json`](../../schemas/siphon-ai.v1.json).

A complete working server built on this SDK:
[`examples/echo-ws-server-node`](../../examples/echo-ws-server-node).
