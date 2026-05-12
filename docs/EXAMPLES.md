# Example WS Servers and Stacks

The protocol in `docs/PROTOCOL.md` is the canonical interface. The examples
here are concrete implementations that exercise it, ranging from "the
simplest possible server" to "what a production AI bridge actually looks
like."

## `examples/echo-ws-server-python/`

The reference echo server. Every audio frame received is echoed back on the
same WebSocket. Useful for verifying the SIP → WS → SIP audio loop works
without any AI provider in the loop.

- Single-file Python: `server.py`, ~150 lines.
- Built into the `docker compose up` demo stack — the daemon connects to
  it automatically when you run from `docker/compose.yaml`.
- Logs every control message (`start`, `hangup`, `dtmf`, `clear`,
  `speech_started`, …) at INFO so you can watch the protocol flow.
- Has a `/healthz` HTTP endpoint that short-circuits the WS upgrade —
  used by the compose healthcheck.

Run standalone:

```sh
cd examples/echo-ws-server-python
pip install -r requirements.txt
python server.py --bind 0.0.0.0:8765
```

Point `[bridge].ws_url = "ws://127.0.0.1:8765/"` at it from the daemon.

## `examples/openai-realtime-bridge-py/`

A working WS server that bridges every accepted call into OpenAI's
[Realtime API](https://platform.openai.com/docs/guides/realtime), so the
caller talks to an AI assistant. Read this if you want to know what a
production bridge looks like; copy it if you want a starting point.

- Translates 16 kHz PCM16 bridge frames into the 24 kHz base64-PCM frames
  the OpenAI Realtime endpoint expects.
- Demonstrates server-side VAD handling — the bridge does the speech
  detection, the AI provider does the listening decision.
- Shows how `BridgeIn::Clear` from the bridge becomes a "interrupt the
  current response" message into the AI session (barge-in).
- Has a smoke test that runs without hitting OpenAI by stubbing the
  upstream socket.

Requires an `OPENAI_API_KEY`. See the README inside the directory for
the full setup.

## `examples/homer-stack/`

A local Homer + heplify-server + Postgres stack via `docker compose up`,
plus a daemon config (`siphon-ai-hep.toml`) that ships HEP3 at it.
Demonstrates the end-to-end flow:

```
siphon-ai ──HEP3──► heplify-server ──SQL──► Postgres ──REST──► homer-app UI
```

Use this to validate `[hep]` config locally before pointing the daemon at a
production Homer. The README walks through what to look for in the Homer
call-flow view, what each chunk type contains, and how the SIP `Call-ID`
correlates SIP messages, RTCP, and SiphonAI's own application chunks.

See `docs/HEP.md` for the architecture and where each chunk type comes from.
