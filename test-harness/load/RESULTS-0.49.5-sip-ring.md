# SIP-ladder ring under load — 0.49.5, 203 concurrent

Run 2026-08-19 against the **shipped 0.49.5 `.deb`** (sha256
`6bf88962f…`, the artifact the reference node runs), driven by the tier-2
FreeSWITCH generator against the node's live `:5060` listener.

Scope: the per-call SIP ladder ring (`DESIGN_SIP_LADDER.md`), which shipped
enabled-by-default in 0.49.3 and had **never been load-tested**. It allocates
per SIP message and holds bounded per-call history, so the questions were
whether its bounds hold, what it costs, and whether it drops anything under
real concurrency.

This is **not** a `LOAD_TEST_PLAN.md` phase. The §4 ramp, §5 arrival-rate
ceiling and §6 soak were not re-run: nothing between 0.48.19 and 0.49.5
touches the media path, so those figures stand.

## Environment

| | |
|---|---|
| Node | Box A, 4 vCPU / 7.9 GB, Debian 13, `0.0.0.0:5060` |
| Generator | Box B FreeSWITCH 1.11.0, `/root/ramp-prod.sh`, 10 cps |
| Ring config | defaults — `sip_ring_size = 50`, `sip_ring_max_messages = 64` |
| Sampling | ring gauges + RSS every 15 s, 40 samples |

Two ramps overlapped: a 50-call run still holding when a 200-call run
started, so peak concurrency is **203**, not 200. That is a fair test of the
bounds and is reported as measured rather than as the number that was asked
for.

## Result — the ring is not a constraint at this scale

| | |
|---|---|
| peak concurrency | **203** |
| peak ring traces | **282** |
| messages captured | 1,671 |
| `dropped_call_cap` | **0** |
| `dropped_trace_cap` | **0** |
| RSS | 20.8 MB idle → **77 MB** peak |
| WARN/ERROR attributable to load | **0** |

Nothing was dropped by either bound. The per-call cap of 64 was never
approached — real calls in this run are 4–8 messages, against the 6–20 the
design predicted — so `truncated` never fired.

Traces track concurrency at roughly **1.3 per call** (67 traces at 50 calls,
282 at 203), the excess being REGISTER refreshes and rejected scanner
INVITEs, which accumulate slowly on a public-IP node: with concurrency flat
at 50, traces still crept 67 → 76 over three minutes.

Total exceeds `MAX_PENDING` (256) legitimately: pending and completed are
capped separately, so the ceiling is `MAX_PENDING + cap_calls` = 306.

### Memory

RSS is the honest caveat. It went 20.8 → 77 MB, but **that is the whole
daemon at 203 concurrent calls**, not the ring: the tier-2 phase 1 run
measured 70 MB at 200 concurrent on 0.48.19, which had no ring at all. The
delta attributable to the ring is within the noise between those two runs and
this one is **not a clean measurement of it** — a ring-on/ring-off A/B at the
same concurrency would be, and was not run.

## What it found instead

### 1. A live call could be evicted by scanner noise (fixed)

The ring bounds a "pending" population — everything without a CDR yet —
least-recently-touched. **An established call is SIP-silent between its ACK
and its BYE**, so its `last_touched` never advances, while scanner INVITEs
and REGISTER refreshes keep arriving with fresh ones. Under one LRU pool the
live call is therefore the *oldest* entry, and would be evicted **before** the
transient noise the bound exists to contain — discarding the ladder of the
call an operator is most likely to be looking at.

The bound was never reached in this run (pending peaked around 230 of 256),
so this did not fire. It was found by reasoning about the trace-growth curve
the samples show, and is fixed in the same change as this write-up: live
calls are a separate population, promoted by the control registry when a call
is accepted, bounded separately at `MAX_LIVE = 512`, and never evicted to
make room for noise. A regression test floods `MAX_PENDING + 100` scanner
traces past one live call and asserts it survives.

### 2. 47 of 250 calls died on WS disconnect — the generator's WS server

```
siphon_ai_calls_total{cause="caller_hangup"} 203
siphon_ai_calls_total{cause="ws_disconnect"}  47
```

**19 % of calls were terminated by the WebSocket server dropping**, not by the
caller and not by the daemon, which logged nothing. This is
`LOAD_TEST_PLAN.md` §1.4 — *"The WS server is probably your bottleneck, not
the bridge"* — happening exactly as written.

It bounds what this rig can measure: past roughly 150 concurrent through that
general-purpose server, a run is measuring the server. The tier-2 results
avoided this by using `paced_sink.mjs`, which is built for load. **Any future
run at this scale should use the paced sink**, and the 47 lost calls here
should not be read as a daemon result.

## Not measured

- **A ring-on/ring-off A/B.** The RSS delta above is not attributable; the
  ring's own cost is still unquantified.
  **CLOSED by `RESULTS-0.49.7-ring-ab.md`**: isolating the ring from media,
  WS and call setup puts it at **~1.9 MB** at this run's shape — about
  2.5 % of the 77 MB measured here, and well inside the variance that made
  this figure unattributable in the first place.
- **The bounds actually tripping.** 203 concurrent did not reach
  `MAX_PENDING`. Reaching it needs ~250 concurrent, or a node up long enough
  for noise alone to fill 256 — on this node's scanner rate, hours.
- **Per-call CPU.** No CPU sampling was taken; this run was about the ring.
- **Anything past 203**, for the WS-server reason above.

## Reproducing

```bash
ssh root@194.195.208.34 "/root/ramp-prod.sh 200 10 600"   # Box B
test-harness/load/ringstat.sh                             # Box A, every 15 s
```

`ringstat.sh` reads `siphon_ai_sip_ring_traces`,
`siphon_ai_sip_ring_messages_total{result}`, `calls_active` and RSS together,
so a trace count is always attributable to a concurrency.
