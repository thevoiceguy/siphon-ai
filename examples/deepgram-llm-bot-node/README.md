# Deepgram + LLM voice agent — Node bot for SiphonAI

A working closed-loop voice agent that speaks SiphonAI's bridge
protocol v1. Caller speaks → Deepgram STT → streaming LLM →
Deepgram TTS → caller hears.

The LLM is any OpenAI-compatible chat-completions endpoint
chosen via env vars at startup (OpenAI, Groq, Anthropic's
OpenAI-compat API, OpenRouter, Fireworks, local Ollama, …).
Defaults to OpenAI `gpt-4o-mini`. See
`docs/BOT_LOCALHOST_SETUP.md` §"Choosing the LLM" for provider
recipes.

Use this as the WS endpoint in `[bridge].ws_url` from
`docs/INSTALL_DEBIAN13.md`, then drive it via the FreeSWITCH
trunk in `docs/FREESWITCH_INTEGRATION.md`. End-to-end demo: a
SIP softphone registered to FreeSWITCH dials `9000` and is
greeted + interrogated by the bot.

## Compared to FreeSWITCH `mod_audio_fork`

If you're porting an audio-fork bot, the substantive changes
are:

| `mod_audio_fork`                            | SiphonAI                                             |
|---------------------------------------------|------------------------------------------------------|
| Binary frames are raw L16 audio.            | **Same.** Raw PCM16 LE, mono. |
| You guess the audio format from FS config.  | **First text msg is `start`** with the audio format. Honour it. |
| Playback via ESL `uuid_broadcast file.wav`. | **No ESL.** Stream PCM16 frames back on the SAME WebSocket. |
| Frame timing is whatever you push.          | **Exactly 20 ms** frames at 50 fps. The bot's `makePlayout` chunks + paces. |
| Barge-in is your problem (VAD locally).     | SiphonAI fires `speech_started`. Bot drops playout + sends `clear`. |
| Hangup via `uuid_kill`.                     | Bot sends `{ "type": "hangup" }`. (This example doesn't initiate hangup — caller does.) |

## Run

```bash
npm install
DEEPGRAM_API_KEY=... OPENAI_API_KEY=... npm start
```

Defaults: binds `0.0.0.0:8080`. Override via `BOT_BIND=…`.
Optional `BOT_SYSTEM_PROMPT=…` and `BOT_GREETING=…` for content.

Point `[bridge].ws_url` at this host's `ws://<bot-ip>:8080/`
and place a call.

## What the bot does on the wire

```text
SiphonAI                                                           Bot
   │                                                                │
   │  text:   start { call_id, audio={pcm16le,8000,1,20}, sip, … }  │
   ├───────────────────────────────────────────────────────────────▶│
   │                                                                │
   │  binary: 320-byte PCM16 frames at 50 fps (caller audio)        │
   ├───────────────────────────────────────────────────────────────▶│
   │                                                                │  STT
   │                                                                │  ↓
   │                                                                │  LLM
   │                                                                │  ↓
   │                                                                │  TTS → 320-byte frames
   │  binary: 320-byte PCM16 frames at 50 fps (TTS audio)           │
   │◀───────────────────────────────────────────────────────────────┤
   │                                                                │
   │  text:   speech_started { ts_ms } (caller interrupts)          │
   ├───────────────────────────────────────────────────────────────▶│
   │                                                                │  drop queue
   │  text:   clear { call_id }                                     │
   │◀───────────────────────────────────────────────────────────────┤
   │                                                                │
   │  text:   stop { reason: "caller_hangup" }                      │
   ├───────────────────────────────────────────────────────────────▶│
   │  WS close (1000)                                               │
```

## Honest limitations

This is a demo bot. Production deployments will want to:

- **Pace better.** `setInterval(pump, 20)` drifts under load. A
  monotonic loop with `Bun.sleepSync` / `tokio`-style timer reset
  is sturdier.
- **Bounded TTS queueing.** A long phrase from the LLM pushes a
  lot of frames at once; the bot's playout queue is unbounded.
  For long calls you'll want backpressure.
- **Handle reconnects.** SiphonAI v1 doesn't do mid-call WS
  reconnect (that's post-v1, see DEV_PLAN §8). The bot crashes
  → the call drops cleanly via the daemon. Fine for v1; budget
  for it if you go to v2.

The wire-format docs in `docs/PROTOCOL.md` are the canonical
contract — when in doubt, read those.
