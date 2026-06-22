# OpenAI cascaded voice bot (STT → LLM → TTS)

A reference SiphonAI WebSocket server that runs a full voice assistant using
**only OpenAI** — Whisper/gpt-4o-transcribe for speech-to-text, chat
completions for the LLM, and gpt-4o-mini-tts/tts-1 for text-to-speech:

```
caller audio ──► STT ──► LLM ──► TTS ──► caller audio
                (OpenAI)  (OpenAI)  (OpenAI)
```

SiphonAI is the SIP↔WebSocket bridge and contains **no AI code** — all of it
lives here, in the WS server, which is exactly the layer it belongs in.

## This vs. `openai-realtime-bridge-py`

| | This bot (cascaded) | `openai-realtime-bridge-py` |
|---|---|---|
| OpenAI API | 3 calls: transcriptions + chat + speech | 1 speech-to-speech Realtime session |
| Pick models independently | ✅ (swap STT / LLM / TTS freely) | ❌ (one Realtime model) |
| See transcript + LLM turns | ✅ | partial |
| Latency | higher (sequential pipeline) | lower |
| Complexity | easy to follow / customise | more moving parts |

Reach for this one when you want control over each stage or to mix providers
(the STT/LLM/TTS calls are isolated and trivial to repoint).

## How it works

- **Transport (protocol v1).** One WebSocket per call. SiphonAI sends a
  `start` JSON message with the audio format, then 20 ms PCM16-LE mono binary
  frames of caller audio. The bot sends 20 ms PCM16 frames back to play into
  the call. See [`docs/PROTOCOL.md`](../../docs/PROTOCOL.md).
- **Turn-taking is done in the bot.** OpenAI transcription is batch (you send
  a finished utterance), so the bot detects end-of-speech itself with
  [`webrtcvad`](https://github.com/wiseman/py-webrtcvad) over the inbound
  20 ms frames. **No SiphonAI VAD config is required** — it works with the
  same default route config as the echo server.
- **Barge-in.** When the caller starts talking while the bot is speaking, the
  bot cancels the in-flight response and sends `clear` to flush SiphonAI's
  outbound queue. (SiphonAI's default `auto_clear` barge-in also does this;
  the explicit `clear` additionally covers `notify_only` deployments.)
- **Greeting on connect.** The bot speaks a greeting immediately on `start`,
  which also satisfies SiphonAI's `server_too_slow` start-deadline (default
  5 s).

## Prerequisites

- Python 3.11+ (the Docker image uses 3.13).
- An OpenAI API key with access to the chosen STT/LLM/TTS models.

```bash
cd examples/openai-pipeline-bot-py
python3 -m venv .venv && source .venv/bin/activate
pip install -r requirements.txt
export OPENAI_API_KEY=sk-...
python3 server.py --bind 0.0.0.0:8080
```

Then point a SiphonAI route at it:

```toml
[[route]]
name = "openai-bot"
[route.match]
any = true
[route.bridge]
ws_url = "ws://127.0.0.1:8080/"
# Default [bridge.barge_in] mode = "auto_clear" gives the snappiest barge-in.
```

Place a call into SiphonAI and talk to the bot.

## Configuration (environment variables)

| Var | Default | Purpose |
|---|---|---|
| `OPENAI_API_KEY` | *(required)* | OpenAI auth. |
| `OPENAI_BASE_URL` | OpenAI | Point STT/LLM/TTS at an OpenAI-compatible base URL. |
| `BOT_STT_MODEL` | `whisper-1` | Transcription model (e.g. `gpt-4o-transcribe`). |
| `BOT_LLM_MODEL` | `gpt-4o-mini` | Chat-completions model. |
| `BOT_TTS_MODEL` | `gpt-4o-mini-tts` | Speech model (e.g. `tts-1`). |
| `BOT_TTS_VOICE` | `alloy` | TTS voice. |
| `BOT_SYSTEM_PROMPT` | *(built-in)* | LLM system prompt. |
| `BOT_GREETING` | *(built-in)* | First thing the bot says. |
| `BOT_AUTH_TOKEN` | *(off)* | Require `Authorization: Bearer <token>` on the upgrade. |
| `BOT_VAD_AGGRESSIVENESS` | `2` | webrtcvad 0–3 (higher = filters more non-speech). |
| `BOT_START_SPEECH_MS` | `120` | Speech needed to open an utterance. |
| `BOT_END_SILENCE_MS` | `700` | Trailing silence that ends a turn. |
| `BOT_PREROLL_MS` | `200` | Audio kept before the trigger so onsets aren't clipped. |
| `BOT_MAX_UTTERANCE_MS` | `30000` | Hard cap so a noisy line can't buffer forever. |

## Notes & limitations

- **Latency.** For clarity the bot waits for the full TTS response before
  playback. To shave latency, stream OpenAI's PCM chunks straight into the
  pacer (resample incrementally with an `audioop.ratecv` state) and/or stream
  LLM tokens into sentence-chunked TTS. Marked in `server.py`.
- **Audio rates.** SiphonAI runs the call at 8 kHz or 16 kHz; OpenAI TTS
  `pcm` output is 24 kHz, resampled down here. STT receives a WAV at the call
  rate.
- **`webrtcvad` wheels.** `requirements.txt` uses `webrtcvad-wheels` so no C
  compiler is needed. On Python 3.13 the `audioop-lts` backport supplies the
  `audioop` module (stdlib through 3.12).
- This is a reference, not a hardened product: no retry/backoff on OpenAI
  errors (it logs and stays on the call), no auth on outbound OpenAI calls
  beyond the API key, single in-memory conversation per connection.

## Tests

`test_smoke.py` covers the pure helpers (WAV wrapping, resampling, 20 ms
framing) and the endpointer state machine with a scripted VAD — offline, no
key needed:

```bash
python3 -m pytest test_smoke.py
```
