# Pipecat voice bot (Deepgram STT → OpenAI LLM → OpenAI TTS)

A SiphonAI WebSocket server built on [Pipecat](https://www.pipecat.ai/) and
the [`siphon-ai-pipecat`](../../sdks/pipecat/) transport:

```
SIP trunk / PBX ──► SiphonAI ──WS──► server.py ──► Pipecat: Silero VAD · Deepgram STT · OpenAI LLM · OpenAI TTS
```

Each call gets its own WebSocket and its own Pipecat pipeline. SiphonAI
contains **no AI code**; every provider call lives in this server.

The bot greets the caller and holds a conversation. Two LLM tools exercise
the transport:

- **`end_call`** says goodbye. The transport waits until the caller has
  *heard* it, then hangs up.
- **`transfer_call`** is enabled when `BOT_TRANSFER_TARGET` is set. It says
  "Transferring you now." and then sends a SIP REFER. The transfer is
  ordered behind the audio, so it doesn't cut the sentence off.

## Run

```bash
cd examples/pipecat-bot-py
python3 -m venv .venv && source .venv/bin/activate
pip install -r requirements.txt

export DEEPGRAM_API_KEY=... OPENAI_API_KEY=...
python3 server.py --bind 0.0.0.0:8080
```

Or, with no provider keys, echo the caller back:

```bash
python3 server.py --bind 0.0.0.0:8080 --echo
```

`--echo` is the quickest way to check the SiphonAI ↔ Pipecat leg on its
own, and it is what CI runs the [protocol conformance testkit](../../docs/CONFORMANCE.md)
against.

## Point SiphonAI at it

```toml
[[route]]
name = "pipecat-bot"
[route.match]
any = true
[route.bridge]
ws_url = "ws://127.0.0.1:8080/ws"

# Let Pipecat decide interruptions (see sdks/pipecat/README.md).
[route.bridge.barge_in]
mode = "notify_only"
```

Then place a call into SiphonAI.

## Configuration

| Variable / flag | Default | Purpose |
|---|---|---|
| `DEEPGRAM_API_KEY` | *(required)* | Deepgram streaming STT. |
| `OPENAI_API_KEY` | *(required)* | OpenAI LLM and TTS. |
| `BOT_LLM_MODEL` | `gpt-4.1-mini` | Chat model. |
| `BOT_TTS_VOICE` | `alloy` | OpenAI TTS voice. |
| `BOT_SYSTEM_PROMPT` | phone-assistant prompt | System instruction. The caller's number is appended. |
| `BOT_GREETING` | "Hi! Thanks for calling…" | Spoken when the call connects. Set it empty to start by listening. |
| `BOT_TRANSFER_TARGET` | *(unset)* | SIP URI for the `transfer_call` tool. The tool is not offered when this is unset. |
| `BOT_INTERRUPT_MIN_WORDS` | *(unset)* | Words the caller must say while the bot is talking before it is interrupted (Pipecat's `MinWordsUserTurnStartStrategy`). Unset means Pipecat's default: any detected voice interrupts. Set it to `2` for SiphonAI's pause mode, so coughs and "mm-hmm" resume the bot. |
| `SIPHON_AUTH_TOKEN` / `--auth-token` | *(unset)* | Require `Authorization: Bearer`, matching SiphonAI's `[bridge].auth_bearer`. |
| `--echo` | off | Echo mode; no providers needed. |
| `LOG_LEVEL` / `--log-level` | `INFO` | Log level. |

To swap providers, change the three service constructors in `build_bot()`.
Any Pipecat STT, LLM or TTS service works, because the transport never
sees them.

## Test

```bash
python3 -m pytest test_smoke.py   # offline: builds the app and the bot pipeline
```
