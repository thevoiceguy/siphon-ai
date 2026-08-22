# Tier 3 rig — a live carrier trunk

`LOAD_TEST_PLAN.md` §10.3 / §10.3.1. Reproduces `../RESULTS-0.49.9-tier3.md`.

> **This dials the public PSTN and bills both legs of every call.** Read
> §10.3's fraud-detection note before running anything, and tell your
> carrier what you are doing. Sudden high-volume origination is the
> signature of toll fraud; losing the trunk costs far more than the test.

## The trombone

Each call is originated through the carrier gateway to **a DID that routes
back to this node**, so it returns as a second inbound leg. Real media
flows both ways, with no human at either end and no AI spend — the outbound
leg bridges to a WS sink, and the returning inbound leg matches a route
that bridges to another.

```
siphon-ai ──INVITE──► carrier SBC ──► carrier routes the DID back
    ▲                                          │
    └────────────── inbound leg ◄──────────────┘
```

**Two channels per call**, one out and one back in. On a 3-channel trunk
that is exactly one call at a time with one channel spare — deliberately,
so a genuine inbound call is not rejected because a test is running. Do not
parallelise without recounting channels.

## Running it

```sh
export SP=/var/tmp/tier3 && mkdir -p $SP
export TOKEN=<admin-role bearer token>
export GATEWAY=<name of the carrier [[gateway]]>
export TO=<DID that routes back to this node>
./tier3_run.sh 60 30        # 60 sequential calls, 30s each
```

`WS` (default `ws://127.0.0.1:8081/?mode=sustain`) is the sink for the
**outbound** leg; the returning leg's sink comes from the daemon's own route
config. `sustain` generates a continuous 50 fps filler, which is what puts
real audio on the wire in both directions.

## Guardrails in the script

A live trunk is not loopback: a run that has stopped working must stop
dialling rather than keep paying to fail.

- **Hard cap** of 200 calls, whatever you pass.
- **Aborts after 3 consecutive failures** — a refused originate, a trombone
  that never closed, or a channel that stayed busy.
- **Never starts on top of existing traffic.** It waits for
  `calls_active == 0`, so a genuine call in progress delays the next sample
  instead of colliding with it and eating the spare channel.
- **Requires both legs up** (`calls_active == 2`) before counting a sample.
  One leg means the carrier took the call and never routed it back, which is
  a failed sample, not a short one.

## Reading the result

Everything comes from the CDRs and the lifecycle webhooks, per call:

| what | where |
|---|---|
| carrier setup latency | outbound CDR `answered_at − started_at` |
| carrier round-trip alone | `outbound_initiated` → the returning leg's `call_start` |
| MOS / jitter / loss | both CDRs' `quality` block |

🪤 **Not `sdp_negotiate_seconds`.** Despite what earlier revisions of §10.3
said, that histogram times `prepare_call` — our own local work — and says
nothing about the carrier.
