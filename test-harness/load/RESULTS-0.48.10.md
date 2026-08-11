# Load & Capacity Results — siphon-ai 0.48.10

Executed against `LOAD_TEST_PLAN.md` on 2026-08-10. Tier 1 only (SIPp on the
same box, loopback, plaintext) — §10's tier 2 and tier 3 are **not** run, so
nothing here is a claim about a network, crypto, or a real carrier.

> **Superseded in two places by `RESULTS-0.48.13.md`.** (1) §6.1's "RSS +1.3 MB
> over the hour" was measured on a process already at 137 MB, whose allocator
> had free pool enough to absorb an hour of churn invisibly; it is not evidence
> that memory is flat, and the per-call figures below are warm-up numbers that
> understate steady state by ~60%. (2) §5's 626 failures at 75 cps do not occur
> with the pps cap disabled, so the "generator-side, not a bridge limit"
> attribution is unconfirmed. (3) §7.2's neural-VAD memory figures are wrong and
> contradict each other — "~6× the per-call memory" below against the "~1.6 vs
> ~0.79 MB/call" bullet, which is 2×. Both per-call numbers are taken at 25
> concurrent, where energy's cost is mostly fixed overhead divided by a small
> N; the real ratio at 200 concurrent is ~8.8× per call. See
> [#472](https://github.com/thevoiceguy/siphon-ai/issues/472). Everything else
> here still stands.

## Environment

| | |
|---|---|
| Hardware | 4 vCPU, 7.9 GB RAM |
| OS / kernel | Debian 13, Linux 6.12.95+deb13-amd64 |
| Version | `siphon-ai 0.48.10` |
| Posture | G.711 µ-law, recording **off**, HEP **off**, energy VAD, no webhooks, no audit |
| `rtp_port_range` | `[41000, 45000]` → 2000-call ceiling, never approached |
| `ulimit -n` | 524288 |
| Generator | SIPp, same box, real PCMU RTP per call |
| WS server | Node sink on `127.0.0.1:8767`, discards audio and returns silence |

Second unprivileged instance on `:5070` / metrics `:9191`, alongside an
untouched production daemon on `:5060`.

## Headline

| Metric | Result |
|---|---|
| **Sustained concurrent calls** | **≥ 250** — every SLO held; not a measured ceiling (see below) |
| **Setup rate** | **≥ 75 cps** at p95 < 0.5 ms — not a measured ceiling |
| CPU at 250 concurrent | 130.4% of one core = **32.6% of 4 vCPU** |
| CPU per call | **0.52%** of one core, *falling* with concurrency |
| RSS at 250 concurrent | 75.6 MB |
| Jitter p95 / MOS p50 / loss | **≤ 20 ms** / **> 4.4** / **0** |
| First audio out (p95) | **19 ms**, identical at every concurrency from 25 to 250 |
| 60-min soak at 200 | RSS +1.3 MB, fds and threads immovable, **zero call drops** |
| Behaviour past the cap | **503 shed, exact, admitted calls unaffected** |
| Cost of turning features on | HEP **free**; recording **+81%** CPU/call; neural VAD **+191%** (§7.2) |

**250 and 75 cps are floors, not ceilings.** The ramp ended where the plan
said to stop, with two-thirds of the CPU idle and quality flat. The rate ramp
ended because the *generator* ran out of headroom, not the bridge. Quote these
as "≥", or rerun with the generator on its own box (§10.1) to find the real
knee.

## §4 — concurrency ramp (10 cps, 5 min per step)

| Concurrent | Setups | CPU (1 core) | CPU/call | RSS | MOS min/avg | Loss ratio | first_audio p95 |
|---|---|---|---|---|---|---|---|
| 1 | 1/1 | 1.1% | 1.10% | 17.1 MB | 4.434 / 4.438 | 0 | 13 ms |
| 25 | 25/25 | 15.1% | 0.60% | 36.1 MB | 4.434 / 4.439 | 0 | 15 ms |
| 50 | 50/50 | 30.7% | 0.61% | 41.1 MB | 4.418 / 4.438 | 0.00001 | 19 ms |
| 100 | 100/100 | 57.4% | 0.57% | 47.8 MB | 4.430 / 4.437 | 0.00001 | 19 ms |
| 150 | 150/150 | 82.1% | 0.55% | 54.6 MB | 4.405 / 4.437 | 0.0000045 | 19 ms |
| 200 | 200/200 | 106.8% | 0.53% | 62.9 MB | 4.429 / 4.437 | 0 | 19 ms |
| **250** | **250/250** | **130.4%** | **0.52%** | 75.6 MB | 4.430 / 4.437 | 0 | 19 ms |

776 calls, **zero failed setups, zero unexplained terminations.** Setup
latency p95 stayed in the bottom histogram bucket (`< 0.5 ms`) throughout.
Per-call CPU *falls* as concurrency rises — fixed overhead amortising, nothing
super-linear anywhere in range. fds are exactly `12 + 3 × calls`; threads never
left 5.

Of §4's seven SLOs, six were measured and one — Playout — has **no data
source in this build** (`outbound_audio_frames_dropped_total` is not
exported). It is unverified, not passed.

## §5 — arrival rate (concurrency pinned at 125)

| Rate | Calls | Successful | Failed | p95 setup | Daemon CPU | Sink CPU |
|---|---|---|---|---|---|---|
| 10 cps | 600 | 600 | 0 | ≤ 0.5 ms | 69.9% | 24.2% |
| 25 cps | 1500 | 1500 | 0 | ≤ 0.5 ms | 76.3% | 26.2% |
| 50 cps | 3000 | 3000 | 0 | ≤ 0.5 ms | 83.3% | 27.8% |
| 75 cps | 4500 | 3874 | **626** | ≤ 0.5 ms | 102.4% | 33.8% (max **67.2**) |

The daemon **accepted 100% of INVITEs at every rate** — 10,626 accepted, zero
rejections, zero 5xx — and setup latency did not move across a 7.5× rate
increase.

The 626 failures at 75 cps are **generator-side**, not a bridge limit. They
map exactly onto `calls_total{cause="tap_ended"}` = 626 and 626 log warnings
of *"no inbound RTP within inactivity window"*: SIPp stopped streaming RTP on
those calls and the watchdog correctly reaped them (§1.3 working as designed).
The WS sink also peaked at 67.2% of a core — three points under §1.4's
invalidation line, i.e. the rig was at the edge of what it can honestly
measure. Anything above 75 cps needs the generator on its own box.

The 200 pps per-source SIP packet cap (§1.2) engaged in 68 distinct seconds
during this phase. It did not cause the failures, but it is active in this
range and it drops SIP silently with no metric.

## §6.1 — 60-minute soak at 200 concurrent

| Criterion | Threshold | Result |
|---|---|---|
| RSS flat after warm-up | ±10 MB | **+1.3 MB** (137.9 → 139.2) |
| fds / threads flat | — | **612 / 5**, unchanged for the hour |
| Call drops during soak | zero | **zero** — `calls_total` identical at T+5 and T+59 |
| Quality at minute 59 | MOS p50 ≥ 4.0, jitter p95 ≤ 30 ms | **MOS p50 > 4.4, jitter p50 5 ms / p95 ≤ 20 ms** |

Minute-59 quality is measured from 129,604 samples in the T+5→T+59 window
alone, not cumulatively. 52.9M frames crossed the WS sink.

**Unresolved:** at the very end, SIPp reported all 200 calls failed and the
daemon terminated all 200 via the inactivity watchdog rather than on BYE,
answering late BYEs with `481`. The 200 pps limiter was **excluded** as a
cause (no warnings during the soak). §6.4 then tore down 250 concurrent calls
cleanly on BYE, which narrows this to SIPp's handling of hour-long pauses
rather than anything general in teardown. Attribution needs a packet capture.
§6.1's "only your own teardowns" criterion is therefore **inconclusive**, not
passed — while every stability criterion above it passed outright.

## §6.4 — degradation past the cap

`max_concurrent = 250`, offered 375 (150%):

| | |
|---|---|
| Admitted | **250** — exactly the cap |
| Shed with `503` | **125** — exactly the excess |
| Silently dropped | **0** |
| Admitted-call quality | MOS min 4.429, **loss 0** / 2,943,713 packets, first_audio p95 19 ms |
| Admitted-call CPU | 0.53%/call — identical to the uncapped 250 run |
| Terminations | 250 `caller_hangup` |

Clean shed at a known cap, no brown-out for admitted traffic.

> Set `drop_after` high when testing this. At its default of 10, the eleventh
> consecutive reject flips the source to **silent drop** and SIPp reports
> timeouts instead of 503s — a clean shed then reads as a hang.

## Finding: ~6–10 KB leaked per completed call

RSS growth tracks **cumulative completed calls**, independent of concurrency
and of frame volume:

| Evidence | Calls | Peak concurrency | RSS change | Per call |
|---|---|---|---|---|
| §5 rate phase | 9,600 | 125 (**below** the 250 high-water) | +60 MB | 6.25 KB |
| 250-step repeat | 250 | 250 (already paid) | +1.5 MB | 6 KB |
| 60-min soak | 200 | 200 | +2.1 MB post-drain | 10.5 KB |

Concurrency is excluded as the driver: §5 ran at half the high-water mark and
still grew 60 MB. Frames are excluded too: the soak's 36 million frames across
only 200 calls moved RSS 1.3 MB, essentially all attributable to the calls
themselves. **The audio hot path is clean; something on the per-call path is
retained.**

At 10k calls/day this projects to ~60–100 MB/day — a restart-or-exhaust
trajectory over weeks, not a cosmetic issue. This is separate from, and on top
of, the legitimate ~0.23 MB/call high-water pooling.

## §7.2 — feature cost deltas

Each variant is one fixed reference point — **200 concurrent, 10 cps, 4-minute
hold** — with the daemon restarted between variants and one knob changed at a
time. §7.2 asks for a re-ramp per feature; the delta a full ramp yields is the
same one a single point at 80% of the knee gives, and anything that moves the
knee moves CPU-per-call here first.

| Variant | CPU/call | vs baseline | CPU (4 vCPU) | RSS | MOS min/avg | Loss | first audio p95 |
|---|---|---|---|---|---|---|---|
| **baseline** (G.711, energy VAD) | 0.54 %/core | — | 27.0% | 56.8 MB | 4.428 / 4.436 | 0 | 19 ms |
| `[recording] mode="always"` | 0.98 %/core | **+81%** | 48.9% | 86.2 MB | 3.530 / 4.420 | 0.00169 | 19 ms |
| `[hep] enabled=true` | 0.54 %/core | **+1%** | 27.2% | 53.6 MB | 4.429 / 4.436 | 0 | 19 ms |
| `vad="neural"` | 1.57 %/core | **+191%** | **78.4%** | **330.9 MB** | 4.266 / 4.412 | 0.00240 | **198 ms** |

All four ran 200/200 calls with zero failed setups and `caller_hangup` on every
termination. Read the rows as costs, not as knees.

**Recording — +81% CPU, and it is the first thing here that touches media.**
Mean CPU nearly doubles and the peak is burstier still (319.6% of one core
against baseline's 117.2%), consistent with WAV writer flushes rather than
steady load. It is also the only variant to lose packets at a rate the CDRs
notice: 4,014 lost of 2.37M (0.169%) and a MOS floor of 3.530 against
baseline's 4.428. Both stay inside §4's SLOs — loss ≤ 1%, MOS *p50* ≥ 4.0, and
the average holds at 4.420 — so this is not a failure, but "recording is free"
is not supportable either. 200 four-minute calls wrote 1.2 GB. Recording to the
same spindle as anything latency-sensitive deserves thought.

**HEP — free, but only the half this rig can exercise.** +1% CPU is inside
sampling noise, and quality is unchanged. The caveat matters: the sink received
1,480 packets across 200 calls, ≈7.4 per call, which is SIP plus one CDR chunk
per call and essentially no RTCP. **SIPp's `rtpstream` sends no RTCP**, so the
per-RTCP-event UDP fan-out that §7.2 expected to be the cost was never
generated. What is measured here is that HEP's SIP + log + CDR path is free.
The RTCP path is untested and this number must not be quoted as if it covered
it — a real endpoint sending RR every 5 s over a 4-minute call would multiply
the packet count by roughly an order of magnitude.

No drop-rate line accompanies this row because **there is no metric to read**:
`siphon_ai_hep_*` is documented in five places and exported nowhere
([#460](https://github.com/thevoiceguy/siphon-ai/issues/460)). The plan's §8
trap that HEP-on runs "need their own drop-rate line" is currently impossible
to satisfy.

**Neural VAD — the most expensive feature by a wide margin, as §7.2 predicted.**
Nearly triple the per-call CPU and ~6× the per-call memory. At 200 concurrent it
draws 78.4% of all four cores, which is §4's ≤80% headroom SLO almost exactly,
and quality degrades the way saturation predicts: first audio p95 goes 19 ms →
198 ms (max 373), loss appears, the MOS floor drops to 4.266. **At 25 concurrent
the same build is clean** — first audio p95 18 ms, MOS min 4.432 — so this is
saturation, not an inherent latency cost of the model.

The knee therefore moves: **≥250 concurrent on energy VAD, below 200 on
neural**, on this hardware. Two measurements worth carrying separately, both
taken at 25 concurrent where nothing contends:

- **CPU: +1.47 %/core per call** attributable to the model — ~470 µs per 32 ms
  window, against the ~60–80 µs `docs/CONFIG.md` quotes
  ([#461](https://github.com/thevoiceguy/siphon-ai/issues/461)).
- **Memory: ~1.6 MB/call**, against energy's ~0.79 MB/call. The cost is *per
  call*, not a one-time model load, and the model loads on the **first call**,
  not at startup — an idle neural-VAD daemon sits at 14.6 MB and tells you
  nothing about its footprint.

### Not run

- **`[recording.storage]` upload** — §7.2 asks for the effect on teardown
  latency. RC-08 proved the upload path works, including spool durability
  across a daemon restart, but it has never been run under load.
- **Opus** — the rig cannot generate it. `call_with_audio.xml` replays a PCMU
  pcap through SIPp's `rtpstream`; an Opus row needs an Opus pcap and an SDP
  offer to match, which is a rig change rather than a run.

## Corrections this run forced back into the plan

- **§4/§5 setup-latency SLO moved 250 ms → 200 ms.** `SDP_NEGOTIATE_BUCKETS`
  tops out at a finite `0.2`, so a 250 ms threshold sat above the highest
  resolvable value — satisfiable but never falsifiable.
- **§1.2 gained the 200 pps per-source cap**, which disabling
  `[sip.admission]` does not affect.
- **§4/§3 metric names corrected**: it is `rtp_rx_jitter_ms`, not
  `rtp_jitter_ms`; `rtp_packet_loss_ratio`, `ws_connect_seconds` and
  `outbound_audio_frames_dropped_total` are **not exported** in 0.48.10.
- **§6.3's RSS criterion rewritten.** "Within 10% of idle" can never hold for
  pools sized to peak concurrency; it was replaced with the two tests that
  actually separate pooling from a leak.
- **§7.2 no longer demands a full re-ramp per feature** — a single reference
  point at 80% of the knee yields the same delta in a fraction of the time —
  and now says to take the *marginal* cost at low concurrency, because at 80%
  of the knee contention understates per-call cost and overstates latency
  impact. Neural VAD measured +1.03 %/core at 200 concurrent and +1.47 at 25.
- **§7.2's "watch HEP drop metrics" and §8's HEP drop-rate trap are struck.**
  No such metric exists (#460).
- **§7.2 gained the caveat that a loopback generator cannot price HEP's RTCP
  fan-out**, because SIPp sends no RTCP.

## Not measured

Multi-node scaling; TLS + SRTP; any network path (all loopback); mid-call WS
reconnect; anything beyond one hour. Of §7.2's feature deltas, recording, HEP
and neural VAD are measured above; **Opus and `[recording.storage]` under load
are not**. `siphon_ai_calls_active` does not exist until the first call, so
alerts on it must handle `absent()`; `siphon_ai_hep_*` does not exist at all
(#460).
