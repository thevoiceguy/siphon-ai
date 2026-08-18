# 8-hour convergence soak — RSS stops growing at hour 4

Run 2026-08-17/18 against the **shipped 0.48.19 `.deb` binary**, 200 concurrent
calls held for **8 hours**.

This closes the one item `RESULTS-0.48.13.md` left open under "Not measured":

> **Convergence.** Hour 2 of the two-hour run still added 6 MB. Growth is
> decelerating sharply and the reuse test says the memory is reusable, but
> nothing here shows it actually stopping, and no run has gone past two hours.

It stops. **Hours 5–8 added 1.6 MB in total**, and file descriptors returned to
their pre-load baseline exactly.

## Environment

| | |
|---|---|
| Under test | `siphon-ai 0.48.19` (shipped deb) on Box A, 4 vCPU / 7.9 GB, Debian 13 |
| Posture | G.711 µ-law, recording off, HEP off, energy VAD, no webhooks, `udp_rate_limit_pps = 0` |
| Load | 200 concurrent, 10 cps ramp, held 28,800 s |
| Generator | **FreeSWITCH 1.11.0 on Box B**, `tier2/ramp.sh` |
| WS sink | `paced_sink.mjs`, `127.0.0.1:8767` |
| Sampling | every 5 min, 96 samples (`tier2/soak8h.sh`) |

**Posture note, and it matters for reading the numbers.** The generator is
FreeSWITCH over the network, *not* SIPp on loopback as in §6.1 — deliberately,
so that nothing competed for eight hours with the process whose memory was being
measured. Absolute RSS is therefore **not** comparable to the published
54.0 → 87.7 MB curve; the *shape* is what this run is for. Per-call CPU over this
path is ~0.650 % against 0.53 % on loopback (`RESULTS-tier2.md`).

## The curve

| | RSS | Δ |
|---|---|---|
| pre-load (no calls) | 16 MB | |
| t+0 | 50.9 MB | |
| t+60 | 144.0 MB | +93.1 |
| t+120 | 197.3 MB | +53.3 |
| t+180 | 205.2 MB | +7.9 |
| t+240 | 221.8 MB | +16.6 |
| t+300 | 222.0 MB | **+0.2** |
| t+360 | 222.1 MB | **+0.1** |
| t+420 | 222.9 MB | **+0.8** |
| t+475 | 223.4 MB | **+0.5** |

| window | change | max−min |
|---|---|---|
| last 60 min | +1.10 MB | 1.10 |
| last 120 min | +1.30 MB | 1.30 |
| last 180 min | +1.40 MB | 1.40 |
| last 240 min | **+1.60 MB** | **1.60** |

Growth is **front-loaded and then over**: 93 MB in hour 1, 53 in hour 2, and
then a total of 1.6 MB across the final four hours. That the 4-hour change and
the 4-hour max−min spread are the *same* number (1.60 MB) matters — it means the
tail is a slow monotone creep, not the burst pattern §6.3 documented earlier in
the run. Hours 3 and 4 (+7.9 then +16.6) are the last of the bursts, and they
are exactly why a 2-hour run could not answer this: at t+120 the curve looked
like it was still climbing steeply, and at t+180 a reader would have called it
converged one hour before a 16 MB burst.

## §6.3 leak audit

| | pre-load | post-drain |
|---|---|---|
| fds | **12** | **12** |
| threads | 6 | 5 |
| `calls_active` | 0 | 0 |
| RSS | 16 MB | 223 MB |

**File descriptors return to the baseline exactly** — the check that actually
distinguishes a leak from allocator retention, and the same result tier 1 got at
one hour. Retained RSS after drain (223 MB) is essentially the loaded figure,
consistent with 0.48.13's reuse test: the memory is held, not lost, and a second
load reuses it.

## Invariants over 96 samples

`calls_active` was **exactly 200** in every sample; `fds` **612** in every sample
(`12 + 3 × 200`, the same formula every tier-1 run produced); `threads` **5**
throughout. **Zero `WARN` or `ERROR` from the daemon in eight hours.** All 200
calls ended `caller_hangup`.

CPU averaged **128.2 %** of one core (min 123.7, max 141.4) with no upward trend.
The single 141.4 % sample at t+420 is isolated — the eleven samples after it read
124–132 % — and is noted only because it was flagged live before the run
finished; it is measurement noise, not a late-run regression.

## Incidental: this is a much stronger §6.2

Tier 1's long-call test was **1 call for 1 hour**. This is **200 calls for 8
hours**, and both clocks hold:

- **RTP arrival: −11 ppm** across 6.92 h of steady state (249,087,221 packets
  received against 249,090,000 expected at 50 fps × 200 calls, timed by the
  samples' own ISO stamps). Tier 1's §6.2 measured 28 ppm on its single call.
- **Call duration: 28,799,929 ms mean** against a 28,800,000 ms target — 71 ms
  under across 8 hours, with a 1,001 ms spread over 200 calls. (This is
  agreement between the generator's scheduler and the daemon's own timestamps,
  not a measure of playout clock; the RTP figure above is the clock number.)

**Quality over the full 8 hours**: 1,151,836 jitter samples, mean **0.276 ms**,
96.7 % ≤1 ms; MOS-estimate **100 % above 4.4**, mean 4.441. No degradation trend
across the run.

## Verdict

**RSS converges.** Growth is front-loaded, ends by roughly hour 4, and the last
four hours are flat to within 1.6 MB. Combined with fds returning exactly to
baseline and 0.48.13's reuse test, the behaviour is warm-up retention, not a
leak. **RSS remains a poor §6.1 pass criterion** — a 60-minute run samples only
the steep part of this curve — and that is why §6.1 does not gate on it.

## Not measured

- **Beyond 8 hours.** The curve is flat for four hours; multi-day behaviour is
  still untested.
- **Under a different posture.** Recording on, HEP on, or neural VAD would each
  change the memory profile; §7.2 covers their cost, not their convergence.
- **What the retained 223 MB is.** This run shows growth stopping, not what the
  memory holds. That needs `heaptrack`, and nothing here now justifies it.
