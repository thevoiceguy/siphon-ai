# Echo WS Server (Python)

Reference WebSocket server for the SiphonAI bridge protocol v1
(`docs/PROTOCOL.md`). Echoes every audio frame back into the call;
logs control messages.

This is the simplest possible compliant server — useful as a connectivity
smoke test, a starting point for your own integration, and the loop-
closing endpoint in CI tests.

## Setup

```sh
cd examples/echo-ws-server-python
python3 -m venv .venv
source .venv/bin/activate
pip install -r requirements.txt
```

## Run

```sh
python3 server.py --bind 0.0.0.0:8080
```

Then point SiphonAI's config at it:

```toml
# siphon-ai.toml
[bridge]
ws_url = "ws://127.0.0.1:8080"
```

## CLI flags

| Flag | Default | Purpose |
|---|---|---|
| `--bind HOST:PORT` | `0.0.0.0:8080` | Listen address. |
| `--delay-ms N` | `0` | Delay each echo by N ms. Useful for testing barge-in / `clear` semantics. |
| `--auth-token TOKEN` | (off) | Require `Authorization: Bearer TOKEN` on the upgrade. |
| `--echo-marks` | off | Send a `mark` event back after `start`. Used by SiphonAI's protocol smoke tests; do not enable in production echo deployments. |
| `--log-level LEVEL` | `INFO` | One of `DEBUG`, `INFO`, `WARNING`, `ERROR`. |

## What you'll see

```
2026-05-04 14:30:00,123 INFO echo-ws: listening on ws://0.0.0.0:8080  (subprotocol=siphon-ai.v1, auth=off, delay_ms=0)
2026-05-04 14:30:01,456 INFO echo-ws: connect peer=('127.0.0.1', 51992) subprotocol='siphon-ai.v1'
2026-05-04 14:30:01,460 INFO echo-ws: start call_id=siphon-7f3a9b21 version=1 from=+13125551212 to=5000 rate=8000 ch=1 frame_ms=20 sip_call_id=abc123@pbx.example.com
2026-05-04 14:30:42,001 INFO echo-ws: stop call_id=siphon-7f3a9b21 reason=caller_hangup
2026-05-04 14:30:42,002 INFO echo-ws: connection closed cleanly
2026-05-04 14:30:42,003 INFO echo-ws: done peer=('127.0.0.1', 51992) call_id=siphon-7f3a9b21 frames_echoed=2050 bytes_echoed=656000
```

## What it deliberately doesn't do

- **No AI logic.** This is the reference for the *transport*. Building a
  voice agent on top is your job.
- **No `hangup` / `transfer` / `send_dtmf` / per-call `mark`.** The echo
  server has nothing meaningful to say; it just bounces audio. See
  `examples/openai-realtime-bridge-py/` (post-v1) for a richer example.
- **No streaming or fragmentation.** One WS message = one JSON object or
  one audio frame, per spec §2.

## Testing

Run the bundled smoke test (also validates the protocol shape):

```sh
python3 -m unittest test_smoke.py
```
