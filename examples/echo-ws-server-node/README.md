# echo-ws-server-node

Reference echo WS server for the SiphonAI bridge protocol v1, built on the
in-repo TypeScript SDK ([`sdks/typescript`](../../sdks/typescript)). It
echoes every audio frame back to the caller — point a softphone at SiphonAI
and you hear yourself.

This is the Node twin of
[`examples/echo-ws-server-python`](../../examples/echo-ws-server-python)
(which carries the SIPp test-harness knobs; this one stays minimal).

## Run

```bash
# One-time: build the in-repo SDK (npm links `file:` deps rather than
# packing them, so the SDK needs its own install + tsc build first).
(cd ../../sdks/typescript && npm install)

npm install
node server.mjs --bind 0.0.0.0:8080
```

Then point a SiphonAI route at `ws://your-host:8080/` and place a call
(see the repo root README for the full local stack).

## Options

| Flag | Meaning |
|---|---|
| `--bind HOST:PORT` | address to listen on (default `0.0.0.0:8080`) |
| `--auth-token TOK` | require `Authorization: Bearer <TOK>` on the upgrade |
| `--echo-marks` | send a `mark` event back after `start` (protocol smoke tests) |
| `--delay-ms MS` | echo each audio frame back after this many ms |

`GET /healthz` returns 200 for container probes.
