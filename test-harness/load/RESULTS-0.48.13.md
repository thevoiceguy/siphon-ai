# Load & Capacity Results — siphon-ai 0.48.13

Executed against `LOAD_TEST_PLAN.md` on 2026-08-11. Tier 1 only (SIPp on the
same box, loopback, plaintext) — §10's tier 2 and tier 3 are **not** run, so
nothing here is a claim about a network, crypto, or a real carrier.

This is a re-run of `RESULTS-0.48.10.md` on the same hardware, in the same
posture, after four releases (0.48.11 → 0.48.13). Read the two side by side:
the point of this run is the deltas, and the largest of them is in memory.

## Environment

| | |
|---|---|
| Hardware | 4 vCPU, 7.9 GB RAM |
| OS / kernel | Debian 13, Linux 6.12.95+deb13-amd64 |
| Version | `siphon-ai 0.48.13` |
| Posture | G.711 µ-law, recording **off**, HEP **off**, energy VAD, no webhooks, no audit |
| `[sip].udp_rate_limit_pps` | **0 (disabled)** — see below |
| `rtp_port_range` | `[41000, 45000]` |
| Generator | SIPp, same box, real PCMU RTP per call |
| WS server | Node sink on `127.0.0.1:8767`, discards audio and returns silence |

Second unprivileged instance on `:5070` / metrics `:9191`, alongside an
untouched production daemon on `:5060`.

**The one deliberate difference from the 0.48.10 run** is
`udp_rate_limit_pps = 0`. On 0.48.10 that cap was hard-coded at 200/sec and
engaged in 68 distinct seconds of the §5 rate ramp, silently dropping SIP
mid-measurement (#459, fixed in 0.48.11 — the cap is now configurable and
counted). Disabling it is what makes §5 a measurement of the bridge rather
than of the transport's packet cap. Every phase below ran with it disabled;
the `stream_rate_limit_fps = 200` line in the startup log is a separate
limiter that does not apply to UDP.

## Headline

| Metric | Result | vs 0.48.10 |
|---|---|---|
| **Sustained concurrent calls** | **≥ 250** — every SLO held; not a measured ceiling | same |
| **Setup rate** | **≥ 75 cps** at p95 ≤ 0.5 ms, **zero failures** | 626 failures at 75 cps → **0** |
| CPU at 250 concurrent | 134.5% of one core = **33.6% of 4 vCPU** | 130.4% (noise) |
| CPU per call | **0.54 %/core**, *falling* with concurrency | identical |
| Jitter p95 / MOS p50 / loss | **≤ 20 ms** / **> 4.4** / **0** | identical |
| First audio out (p95) | **19 ms**, flat from 25 to 250 concurrent | identical |
| 60-min soak at 200 | zero drops, fds/threads immovable, **all 200 torn down on BYE** | teardown anomaly **resolved** |
| Memory at 200 concurrent | **0.38 MB/call** at steady state (~50 min to reach) | 0.23 MB/call at 5 min — *understated* |
| Per-completed-call RSS growth | **gone** (#458 fixed) | was 6–10 KB/call |
| Behaviour past the cap | **503 shed, exact, admitted calls unaffected** | identical |

**250 and 75 cps remain floors, not ceilings.** The ramp ended where the plan
said to stop, with two-thirds of the CPU idle and quality flat.

## §4 — concurrency ramp (10 cps, 5 min per step)

| Concurrent | Setups | CPU (1 core) | CPU/call | RSS | MOS min/avg | Loss | first_audio p50/p95 |
|---|---|---|---|---|---|---|---|
| 25 | 25/25 | 17.3% | 0.69% | 22.5 → 25.8 MB | 4.433 / 4.439 | 0 / 369,462 | 11 / 17 ms |
| 50 | 50/50 | 32.1% | 0.64% | 28.3 → 31.5 MB | 4.430 / 4.438 | 0 / 738,925 | 11 / 19 ms |
| 100 | 100/100 | 58.5% | 0.58% | 40.1 → 46.6 MB | 4.430 / 4.437 | 0 / 1,478,070 | 10 / 19 ms |
| 150 | 150/150 | 87.3% | 0.58% | 53.1 → 62.2 MB | 4.428 / 4.436 | 0 / 2,217,301 | 12 / 19 ms |
| 200 | 200/200 | 113.9% | 0.57% | 66.8 → 72.1 MB | 4.429 / 4.436 | 0 / 2,956,287 | 13 / 19 ms |
| **250** | **250/250** | **134.5%** | **0.54%** | 78.2 → 83.9 MB | 4.428 / 4.436 | 0 / 3,695,422 | 12 / 19 ms |

775 calls, **zero failed setups, zero unexplained terminations** — every step
ended `{'caller_hangup': N}`. **Zero packets lost across 11.4 million
received.** Per-call CPU falls as concurrency rises, as on 0.48.10.

The RSS column is one daemon across sequential steps, so it is cumulative and
each step's "first" includes the previous step's high-water. Do not read it as
per-step cost — §6.3 below measures that properly.

`dialogs_active` (new gauge, 0.48.13) returned to **0** after the ramp. On
0.48.10 the dialog store only ever grew.

## §5 — arrival rate (concurrency pinned at 125)

| Rate | Calls | Successful | Failed | p95 setup | Admission |
|---|---|---|---|---|---|
| 10 cps | 600 | 600 | **0** | ≤ 0.5 ms | never engaged |
| 25 cps | 1500 | 1500 | **0** | ≤ 0.5 ms | never engaged |
| 50 cps | 3000 | 3000 | **0** | ≤ 0.5 ms | never engaged |
| 75 cps | 4500 | 4500 | **0** | ≤ 0.5 ms | never engaged |

9,600 calls, **zero failures at every rate**, and setup latency did not move
across a 7.5× rate increase.

**This clears the 0.48.10 run's 626 failures at 75 cps.** That run attributed
them to the generator (SIPp stopping RTP, the watchdog reaping via
`tap_ended`) and treated 75 cps as the rig's edge. With the pps cap disabled
they do not occur at all. Two candidate explanations survive — the cap
dropping SIP was the real cause, or the rig simply had more headroom this run
— and this data does not separate them. What it does establish is that the
0.48.10 report's "generator-side, not a bridge limit" attribution is **not
confirmed**, and 75 cps is clean.

## §6.1 — 60-minute soak at 200 concurrent

| Criterion | Threshold | Result |
|---|---|---|
| Call drops during soak | zero | **zero** — 200/200, `sipp_exit=0` |
| Terminations | your own teardowns only | **200 × `caller_hangup`** |
| fds / threads flat | — | **612 / 5**, unchanged for the hour (`612 = 12 + 3×200`) |
| Quality at minute 59 | MOS p50 ≥ 4.0, jitter p95 ≤ 30 ms | **MOS 100% > 4.4; jitter p50 ≤ 10 ms, p95 ≤ 20 ms** |
| RSS flat after warm-up | ±10 MB | **+36.8 MB (53.0 → 89.8)** — see §6.3 |

Minute-59 quality is measured from the **129,604 samples in the T+5 → T+59
window alone**, not cumulatively. Zero packets lost and zero out-of-order
across 179,866 received per call. The daemon logged **nothing at all** between
the last setup and the first BYE — no warnings, no errors, for the full hour.

**The 0.48.10 run's one unresolved item is resolved.** That soak ended with
all 200 calls reaped by the inactivity watchdog and late BYEs answered `481`,
which left §6.1's "only your own teardowns" criterion inconclusive. On 0.48.13
all 200 tore down on BYE, and the criterion passes outright.

Only the RSS criterion fails, and it fails for a reason that is not a defect
in the daemon. That is the subject of §6.3.

## §6.3 — what RSS actually does

The soak's +36.8 MB was worth chasing down, because on its face it looks like
a per-frame leak: concurrency was pinned at 200 for the whole hour with **zero
call churn**, so nothing that grows can be blamed on completed calls (#458,
fixed in 0.48.13). Five measurements, all on this hardware:

| Measurement | Daemon | Result |
|---|---|---|
| Idle baseline, fresh process | 0.48.13 | **14.9 MB** |
| 50 calls × 15 min, fresh | 0.48.13 | 30.2 MB (**+2.7 MB** after warm-up) |
| 200 calls × 15 min, fresh | 0.48.13 | 60.0 MB (**+2.9 MB** after warm-up) |
| 200 calls × 15 min, fresh | 0.48.10 | 69.0 MB (**+16.9 MB** after warm-up) |
| 200 calls × 60 min, fresh (the soak) | 0.48.13 | 90.2 MB (**+36.8 MB**) |
| **200 calls × 15 min into the already-loaded 90.2 MB process** | 0.48.13 | **92.4 MB (+2.2 MB)** |

**It is not a leak.** The last row is the test that settles it. The soak
daemon sat idle for ten hours at exactly 90.2 MB with zero calls, 12 fds and
`dialogs_active = 0`. Putting a *complete second load* — another 200 calls,
another 15 minutes, another ~9 million frames — through that same process
grew it by **2.2 MB**, where a fresh daemon needs ~45 MB for the identical
work. Roughly 95% of the retained memory was handed straight back to the new
calls. The working set is bounded and reused; what RSS is showing is memory
that is free to the program but never returned to the OS.

The shape confirms it. That 90.2 MB is `RssAnon` 76.7 MB living in two mmap'd
glibc secondary arenas (45 MB and 20.5 MB regions, nearly fully resident),
with no `[heap]` segment at all. Growth arrives in **discrete steps** — flat
for eight minutes, then +4 MB in sixty seconds, then flat again — with no
correlate in daemon CPU, generator CPU, sink CPU, or system load. That is
arena expansion under allocation churn, not a live set that grows. The tap's
outbound queue holds one `Vec<u8>` per frame (`push_audio(bytes: Vec<u8>)`),
so a 200-call daemon is doing on the order of 10,000 alloc/free pairs per
second; glibc grows arena headroom to absorb it and does not trim.

Two hypotheses were tested and killed:

- **Metrics scraping.** An idle daemon with zero calls took 1,372 `/metrics`
  scrapes at 4 Hz: +1.1 MB across the first ~400, then flat. The soak's 240
  scrapes cannot produce 37 MB.
- **The new dialog reaper** (0.48.13, #458). It only acts on retirements, and
  during a pinned-concurrency hold there are none.

### What to budget

Per-call memory is **~0.2 MB/call plus ~5 MB fixed** at 15 minutes — a linear
fit through the 50-call and 200-call arms — but the steady-state figure after
an hour at 200 concurrent is **0.38 MB/call**. The 5-minute-per-step figures
in `RESULTS-0.48.10.md` (~0.23 MB/call) are the *warm-up* number, and they
understate the plateau by about 60%. Size on the plateau: ~110 MB at 250
concurrent, which is nothing on any real box — the point is not the magnitude
but that the smaller number is the one an operator would otherwise quote.

**0.48.13 is strictly better than 0.48.10 here**, which is the opposite of
what the soak's raw number suggests: matched fresh 15-minute arms at 200
concurrent grew +2.9 MB against +16.9 MB and finished 9 MB lower. The bursty
ratcheting is present in both; 0.48.10 simply happened to take one of its
bursts inside the measurement window and 0.48.13 did not. **The soak's
+36.8 MB against 0.48.10's published +1.3 MB is not a regression** — the
0.48.10 soak ran on a process already at 137 MB from earlier phases, whose
allocator had ample free pool to absorb an hour of churn invisibly. Compare
warm to warm and the numbers agree: +1.3 MB there, +2.2 MB in the reuse row
above.

### Consequences for the plan

§6.1's "RSS flat within ±10 MB after a 5-minute warm-up" is not a criterion
this daemon can meet, and failing it means nothing. The warm-up is closer to
50 minutes than 5, and growth arrives in bursts that a short run may or may
not catch — the two 15-minute arms above differ by 14 MB on identical work
purely by which side of a burst they landed. Both §6.1 and §6.3 have been
rewritten around the reuse test, which is cheap, deterministic, and actually
distinguishes untrimmed arena from a leak.

## §6.4 — degradation past the cap

`max_concurrent = 250`, offered 375 (150%):

| | |
|---|---|
| Admitted | **250** — exactly the cap |
| Shed with `503` | **125** — exactly the excess, all logged `admission rate limit` |
| Silently dropped | **0** |

Clean shed at a known cap, identical to 0.48.10.

## Not measured

- **§6.2 long-call soak** (1 call, 1 hour) — not run this pass.
- **§7.2 feature-cost deltas** — not re-run; the 0.48.10 figures stand
  (recording +81% CPU, neural VAD +191%, HEP free).
- **Playout SLO** — still no data source. `outbound_audio_frames_dropped_total`
  exists in the code but is not exported unless it increments, so "zero
  dropped" and "not instrumented" are indistinguishable. Unverified, not
  passed — unchanged from 0.48.10.
- **A second hour at 200 concurrent.** The plateau is inferred from the soak
  flattening at 90.1–90.2 MB over its final six minutes, holding that value
  for ten idle hours, and absorbing a full second load for +2.2 MB. That is
  strong, but it is not the same as having watched hour two.
- **Tier 2 / tier 3** — generator on its own box, TLS/SRTP, real carrier.
