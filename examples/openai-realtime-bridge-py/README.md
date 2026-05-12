# OpenAI Realtime ↔ SiphonAI bridge (reference)

A ~400-line Python WebSocket server that bridges a SiphonAI phone
call to the OpenAI Realtime API.

This is the **canonical reference** for plumbing SIP calls into a
conversational LLM: half-duplex VAD-driven turns, barge-in via
SiphonAI's `clear` control message, and sample-rate conversion
between the caller's 8/16 kHz and OpenAI's 24 kHz.

## What you get

1. SiphonAI takes the SIP call and connects this server.
2. This server reads the `start` message to learn the caller's audio format.
3. It opens an OpenAI Realtime WS session with server-side VAD enabled.
4. Audio flows in both directions, resampled as needed.
5. When OpenAI's VAD detects the caller starting to speak, this
   server sends `clear` back to SiphonAI so any in-flight TTS is
   interrupted (barge-in).
6. DTMF digits the caller presses are forwarded into the
   conversation as `[caller pressed DTMF digit 'N']` text events.

## Run

```sh
python3 -m venv .venv
.venv/bin/pip install -r requirements.txt

export OPENAI_API_KEY=sk-...
.venv/bin/python server.py --bind 0.0.0.0:8765
```

Then point SiphonAI's `bridge.ws_url` at this server (e.g.
`ws://127.0.0.1:8765/` if running on the same host as the daemon)
and place a SIP call. The model answers as soon as the caller
finishes their opening sentence.

## Flags

| Flag                | Default                        | What                                       |
|---------------------|--------------------------------|--------------------------------------------|
| `--bind`            | `0.0.0.0:8765`                 | Listen address                             |
| `--model`           | `gpt-realtime-2025-10-01`      | OpenAI Realtime model ID                   |
| `--voice`           | `alloy`                        | TTS voice (`alloy`/`echo`/`fable`/`onyx`/`nova`/`shimmer`/`verse`) |
| `--instructions`    | (short helpful-assistant text) | System prompt                              |
| `--vad-threshold`   | `0.5`                          | OpenAI server-VAD speech threshold (0–1)   |
| `--log-level`       | `INFO`                         | Standard Python logging level              |

Environment variables:

- `OPENAI_API_KEY` *(required)* — your API key
- `OPENAI_REALTIME_MODEL` — alternate way to set `--model`

## Architecture

```text
       ┌─────────────┐   PCM16 @ 8 kHz   ┌──────────────────┐   PCM16 @ 24 kHz   ┌──────────────┐
caller │  SiphonAI   │ ─────────────────▶│ openai-realtime- │ ─────────────────▶│   OpenAI     │
   ◀───┤ (SIP + RTP) │                   │   bridge-py      │                   │   Realtime   │
       └─────────────┘ ◀───────────────── └──────────────────┘ ◀───────────────── └──────────────┘
               WS frames        binary audio + JSON control    JSON events + base64 audio
```

The bridge owns:

* **Resampling.** Linear interpolation between caller and model
  rates. Adequate for speech intelligibility; swap for
  `scipy.signal.resample_poly` for production-grade fidelity
  (commented stub at the bottom of `server.py`).
* **Frame slicing.** OpenAI sends variable-length audio chunks;
  SiphonAI expects exactly 20 ms frames (160 samples @ 8 kHz, 320
  @ 16 kHz). The bridge slices and zero-pads as needed.
* **Barge-in.** OpenAI's `input_audio_buffer.speech_started` event
  triggers a `clear` to SiphonAI so the playout queue drops any
  TTS the model was mid-sentence on.
* **DTMF passthrough.** SiphonAI's `dtmf` event becomes a synthetic
  text message in the conversation.

## What this example does NOT do

* **Function calling / tool use.** Add tool defs in the
  `session.update` we send, route `response.function_call_arguments.*`
  events to your handlers. The bridge itself doesn't need to know
  about your tools.
* **Multi-tenancy beyond one process.** This is fine for tens of
  concurrent calls; for hundreds, run multiple processes behind a
  load balancer.
* **Persistent conversation memory.** Each call is a fresh
  session. Wire your own conversation history via
  `conversation.item.create` events on connect if you need it.

## Production swap-ins

* Replace the linear resampler with `scipy.signal.resample_poly` —
  ~5 lines, drops the aliasing on the 8 → 24 kHz upsample.
* Add structured logging (`structlog` or similar) — `LOG.info` is
  fine for a demo but production wants JSON for ingestion.
* Track per-session metrics (frames sent/received, model latency,
  resample CPU) — expose via Prometheus if you want them
  correlated with SiphonAI's own metrics.

## License

This example is provided under the same dual MIT/Apache-2.0 license
as the rest of SiphonAI. Use it as a starting point; rewrite as
freely as the use case demands.
