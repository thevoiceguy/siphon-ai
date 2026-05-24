# Transcription WS Server (Python, Deepgram)

Reference WebSocket server for the SiphonAI bridge protocol v1
(`docs/PROTOCOL.md`) that **transcribes** every call instead of
bridging it to an AI agent. Each call's audio is streamed to
Deepgram's real-time STT and one JSON line per transcript lands on
stdout.

This is the simplest demonstration of SiphonAI's **non-agent** use
case — real-time transcription, call recording, compliance
monitoring, supervisor assist. The voice-bot story is in
`examples/openai-realtime-bridge-py/` (0.1.0 reference); this one is
deliberately read-only.

## What it does

```text
SIP caller ──RTP──► SiphonAI ──PCM16-LE 20ms──► this server ──► Deepgram WSS
                                                       │
                                                       ◄── transcripts ──
                                                       │
                                                       ▼
                                                    stdout
```

No audio is sent back. SiphonAI sees a quiet WS server; the caller
talks, the AI doesn't reply. That's the point.

Downstream consumers (a log shipper, a webhook bridge, an analytics
service, a database loader) tail the JSON-line stream — that's the
integration seam for everything past a screen-printer.

## Setup

```sh
cd examples/transcription-server-py
python3 -m venv .venv
source .venv/bin/activate
pip install -r requirements.txt
export DEEPGRAM_API_KEY="dg_..."   # Deepgram REST API token
```

## Run

```sh
python3 server.py --bind 0.0.0.0:8080
```

Point SiphonAI at it:

```toml
# siphon-ai.toml
[bridge]
ws_url = "ws://127.0.0.1:8080"
```

Place a call, then watch the stdout:

```text
{"ts":"2026-05-23T18:42:11+00:00","call_id":"siphon-7f3a9b21","sip_call_id":"abc123@pbx","text":"Hello, how can I help you today","is_final":true,"speech_final":true,"confidence":0.987,"start_s":1.2,"duration_s":1.8,"words":[…]}
{"ts":"2026-05-23T18:42:18+00:00","call_id":"siphon-7f3a9b21","sip_call_id":"abc123@pbx","text":"I'd like to check the status of my order","is_final":true,"speech_final":true,"confidence":0.991,…}
```

## CLI flags

| Flag                  | Default            | Purpose                                                                 |
| --------------------- | ------------------ | ----------------------------------------------------------------------- |
| `--bind HOST:PORT`    | `0.0.0.0:8080`     | Listen address.                                                         |
| `--model MODEL`       | `nova-3`           | Deepgram model. `nova-2` for the previous generation.                   |
| `--language LANG`     | `en`               | BCP-47 language code.                                                   |
| `--no-interim`        | (interim on)       | Drop interim results; only emit finals (lower update rate, less noise). |
| `--no-smart-format`   | (smart-format on)  | Disable Deepgram smart-format (numerals, punctuation).                  |
| `--auth-token TOKEN`  | (off)              | Require `Authorization: Bearer TOKEN` on the upgrade.                   |
| `--log-level LEVEL`   | `INFO`             | One of `DEBUG`, `INFO`, `WARNING`, `ERROR`.                             |

`DEEPGRAM_API_KEY` is required in the environment.

## Output schema

One JSON object per line on stdout. Fields:

| Field          | Type             | Notes                                                                                 |
| -------------- | ---------------- | ------------------------------------------------------------------------------------- |
| `ts`           | ISO-8601 UTC     | When this transcript was emitted by the server.                                       |
| `call_id`      | string           | SiphonAI bridge call id from the `start` message.                                     |
| `sip_call_id`  | string           | The underlying SIP `Call-ID`.                                                         |
| `text`         | string           | Recognised text. Empty interims are filtered.                                         |
| `is_final`     | bool             | Deepgram considers this a stable result for the audio window.                         |
| `speech_final` | bool             | Deepgram detected end-of-utterance — useful as a turn-boundary signal.                |
| `confidence`   | float            | Deepgram's confidence in the alternative used.                                        |
| `start_s`      | float            | Offset from the call's first audio sample, in seconds.                                |
| `duration_s`   | float            | Length of the audio window this transcript covers, in seconds.                        |
| `words`        | list             | Per-word breakdown (each with `word`, `start`, `end`, `confidence`).                  |

## Swapping providers

The script is intentionally one file with one provider so the
data-flow is legible. To switch:

1. Replace `_open_deepgram` (lines ~115) with a function that opens
   the target's streaming WS and returns a `ClientConnection`-like
   object accepting `send(bytes)`.
2. Replace `_extract_transcript` (lines ~125) with a parser that
   pulls `text` / `is_final` / `confidence` out of the target's
   message shape.

The SiphonAI side and the JSON-line schema stay identical, so
downstream pipelines don't have to know which STT is in use.

For a worked multi-provider abstraction, see
`examples/openai-realtime-bridge-py/` (the 0.1.0 reference). That
example does both STT and TTS with a pluggable provider seam — the
right pattern when you want to switch providers per call or
A/B-test in production.

## Forwarding transcripts elsewhere

The example writes to stdout because that's the minimum viable
integration surface. Pipe it to whatever you want:

```sh
python3 server.py | tee transcripts.jsonl | your-pipeline-runner
```

For an outbound HTTP webhook, add ~20 lines using `aiohttp` (or
`httpx`) in `_emit`. Deliberately not in-tree to keep the example
deps minimal.

## What it deliberately doesn't do

- **No audio reply.** This is the reference for the *observer* shape.
  Adding TTS / agent behaviour is the voice-agent example's job.
- **No `hangup` / `transfer` / `send_dtmf`.** The transcription
  server has no opinion on the call; it just listens.
- **No retry / persistence on Deepgram errors.** If Deepgram closes
  the connection mid-call the server closes the SiphonAI side with
  WS code 1011. Production deployments may want exponential-backoff
  reconnect + buffered re-send — extension point left to the
  consumer.
- **No multi-language detection.** `--language` is fixed per process.
  Per-call language requires either Deepgram's language-detection
  models or a config-driven swap before the WS upgrade.

## Testing against a real call

Easiest: pair with the existing local-dev setup.

```sh
# Terminal 1 — this server
python3 server.py

# Terminal 2 — siphon-ai with local-dev config pointed at us
# (edit configs/local-dev.toml so [bridge].ws_url = "ws://127.0.0.1:8080")
cargo run -p siphon-ai -- --config configs/local-dev.toml

# Terminal 3 — place a test INVITE with SIPp (or a softphone)
sipp -sn uac 127.0.0.1:5070 -m 1 -s 1000
```

Transcripts land on Terminal 1's stdout as the caller speaks.
