# Load & Capacity Test Plan

Companion to `README.md` in this directory. The README explains how to run
the two existing SIPp scenarios; this plan says **what to measure, in what
order, and what "passing" means** — so the run produces numbers we can
publish rather than a subjective "it seemed fine".

**Target rig:** 4 vCPU / dedicated box, nothing else running on it.
**Deliverable:** the results table in §7, suitable for the README and for
answering the first question any VoIP engineer asks — *what's the ceiling,
on what hardware, and how does it behave past it?*

---

## 1. Read this before you touch anything

Four configuration facts will invalidate a run if you miss them. Three of
them make the daemon look worse than it is; one makes it look better.

### 1.1 RTP port range is the real ceiling, not CPU

Every active call holds **two** UDP ports (RTP + RTCP). The range is a hard
cap that has nothing to do with load:

| `rtp_port_range` | Ports | **Max concurrent calls** |
|---|---|---|
| `[40000, 40100]` — shipped `configs/local-dev.toml` | 100 | **50** |
| `[40000, 40500]` — typical production | 500 | **250** |
| `[40000, 42000]` — shipped `configs/soak.toml`, sized for the 500-call test | 2000 | **1000** |

**The `concurrent_burst_500.xml` scenario cannot pass with the config the
README points at** — it exhausts ports at 50 calls and reports failures that
look like the bridge falling over. Size the range to at least `2 ×
target_concurrency` (plus headroom for calls in teardown, which hold their
ports briefly) before running anything in §4.

**Then reserve the range from the kernel, or a run will lose the odd call to
a port it never actually owned.** Every range in the table above sits inside
the default `net.ipv4.ip_local_port_range` (`32768–60999`), so the kernel is
free to hand an RTP port to any socket that does not bind explicitly — a DNS
lookup by any process on the box qualifies. The call that wanted it is
rejected `500`:

```
WARN rejecting INVITE error=forge session error: Network error:
  Failed to bind socket to 0.0.0.0:44134: Address in use (os error 98)
```

Measured **once in 399 calls** on the 0.48.18 soak, and it is not the
daemon's fault — `README.md` calls a 500 here "forge itself is broken",
which sends you looking in the wrong place. Fix it before the run:

```sh
sudo sysctl -w net.ipv4.ip_local_reserved_ports=41000-45000   # = rtp_port_range
```

See issue #504 and `docs/DEPLOY.md`. A run without this is still valid for
everything except a clean zero-failure claim, so record it if you skip it.

**From 0.48.19 a single lost bind no longer fails the call** — forge draws up
to five pairs, logging `RTP port is held outside the pool; drawing another
pair` each time it steps past one, so the `500` above needs *five consecutive*
collisions. Reserve the range anyway: it means the retry never fires, and a
`warn!` that does fire is telling you something real about the host. Against a
pool with half its pairs held, 0.48.18 lost 7 calls in 20 and 0.48.19 lost 1 —
see `RESULTS-0.48.19.md`.

### 1.2 Admission control will rate-limit the whole test

SIPp drives every call from **one source IP**, so `[sip.admission]`'s
per-source token bucket applies to the entire load generator:

- `max_per_sec = 10` caps the test at 10 cps regardless of what the bridge
  could do, and
- after `drop_after` consecutive rejects the limiter **silently drops** —
  SIPp then reports timeouts, not 503s, which reads as a hang.

For capacity runs, either omit `[sip.admission]` entirely or set
`max_per_sec = 0` and `max_concurrent = 0`. Then **prove** it wasn't
interfering: `siphon_ai_invite_admission_total{result="rate_limited"}` and
`{result="dropped"}` must both be **0** at the end of every run. Admission
behaviour is tested deliberately in §6, not accidentally in §4.

**And there is a second limiter that disabling `[sip.admission]` does not
touch.** siphon-rs's UDP transport carries a hard-coded per-source packet
cap (`UDP_RATE_LIMIT_PPS = 200` in `crates/sip-transport/src/lib.rs`) that
silently drops SIP packets from any single source IP above 200 pps. It is
**not configurable** and appears nowhere in `docs/CONFIG.md`. At roughly
four SIP packets per call it starts biting near 50–65 cps from one
generator, which is squarely inside the range §5 asks you to test.

Two things follow. When a run near or above ~50 cps shows odd
retransmissions or timeouts, `grep 'UDP per-source rate limit exceeded'`
the daemon log before blaming the bridge. And treat the count you find as
a floor, not a measure: the warning fires only on the *first* packet over
the limit in each one-second window, and **no metric counts the dropped
packets at all** — so the log tells you in how many distinct seconds the
cap was breached, and nothing tells you how much SIP was discarded.

### 1.3 The inactivity watchdog kills silent calls

`[media].inactivity_timeout_secs` (default 60) tears down a call with no
inbound RTP. A soak that doesn't stream real audio dies at the 60-second
mark and looks like a stability bug. Either stream a pcap (preferred — it
exercises the jitter buffer and codec path, which is most of the per-call
CPU) or set `inactivity_timeout_secs = 0` and label the run
**signalling-only**. The shipped `configs/soak.toml` disables it (and
omits `[sip.admission]` entirely, per §1.2).

### 1.4 The WS server is probably your bottleneck, not the bridge

This is the trap that produces a wrong headline number. The Python echo
server in `examples/echo-ws-server-python` is single-threaded and will
saturate well before the bridge does — you would be publishing *its*
ceiling.

Mitigate all three ways:

1. Use the Node echo server, or a trivial WS sink that discards audio and
   sends silence. **Pace whatever you use against a monotonic origin**
   (`target = t0 + n × 20 ms`), never a bare `setInterval` — see §8.
2. Run the WS server **on a different box**, or at minimum pin it to its own
   core and measure its CPU separately.
3. Record both processes' CPU in every result row. If the WS server is above
   ~70% of a core while the bridge is below it, the run is invalid — say so
   and rerun rather than publishing the number.

---

## 2. Environment to record with every result

Capture this once per session; it goes in the published table.

```sh
nproc; free -m | awk 'NR==2{print $2" MB"}'
uname -r; cat /etc/os-release | grep PRETTY
siphon-ai --version
ulimit -n                    # fds: each call ≈ 4 (SIP share + RTP + RTCP + WS)
sysctl net.core.rmem_max net.core.wmem_max
```

Config knobs that materially change the result — record their values:
`rtp_port_range`, `[media].codecs`, `inactivity_timeout_secs`,
`[recording].mode`, `[hep].enabled`, `[route.media].vad`,
`[sip.admission]`, `[observability]`.

**Baseline posture for §3–§5:** recording **off**, HEP **off**, VAD
**energy** (not neural), no webhooks, no audit. Those are measured as
deltas in §7.2 — mixing them into the baseline is how you get a number you
can't explain.

---

## 3. Phase 0 — per-call cost baseline

**Goal:** know what one call costs before extrapolating.

Run a single 60-second call with audio. Record:

| Quantity | Where |
|---|---|
| RSS delta vs idle | `ps -o rss= -p $(pgrep -f 'siphon-ai --config')` |
| CPU % of one core | `pidstat -p <pid> 5` |
| fds held | `ls /proc/<pid>/fd \| wc -l` |
| Threads | `ls /proc/<pid>/task \| wc -l` |
| Jitter / loss / MOS | `siphon_ai_rtp_rx_jitter_ms`, `siphon_ai_rtp_mos_estimate`; **loss only from the CDR** |
| Setup latency | `siphon_ai_sdp_negotiate_seconds` (`ws_connect_seconds` is **not exported** — verified absent in 0.48.10) |

**Pass:** call completes, MOS ≥ 4.0 on loopback, loss ≈ 0, CDR written with
`cause=caller_hangup`. Per-call RSS and CPU become the denominators for
every projection below.

---

## 4. Phase 1 — concurrency ramp (find the knee)

**Goal:** the headline number — how many concurrent calls hold quality.

Method: step concurrency, hold each step **5 minutes**, and stop at the
first step that breaks an SLO. Do not ramp to failure and report the last
number before the crash; report the last number that held *quality*.

Suggested steps for 4 vCPU: **25 → 50 → 100 → 150 → 200 → 250**, arrival
rate fixed at a gentle 10 cps so setup rate isn't a confound (that's §5).

Per-step SLO — all must hold for the full 5 minutes:

| SLO | Threshold |
|---|---|
| Setup success | 100% reach `200 OK`; `invites_total` == `calls_total` |
| Media quality | `siphon_ai_rtp_mos_estimate` p50 ≥ 4.0; loss p95 ≤ 0.01 **from the CDR** — `rtp_packet_loss_ratio` does not exist |
| Jitter | `siphon_ai_rtp_rx_jitter_ms` p95 ≤ 30 ms (**not** `rtp_jitter_ms`) |
| Playout | `siphon_ai_outbound_audio_frames_dropped_total` **0 for the whole step** — and if not, attribute it before blaming the bridge (see note) |
| Setup latency | `sdp_negotiate_seconds` p95 ≤ 200 ms (see note) |
| Headroom | bridge CPU ≤ 80% of total cores; RSS not growing within the step |
| Not port-capped | concurrency < `rtp_port_range / 2` (else you measured §1.1) |

**The playout SLO is assertable again — and it usually indicts the sink.**
`siphon_ai_outbound_audio_frames_dropped_total` did not exist when this
table was written; it shipped in 0.48.14 (#474) and publishes at zero, so
the row is a real threshold rather than a struck-out one. Read a non-zero
value as a *question*, not a verdict: the counter fires when the WS server
hands the bridge audio faster than realtime and the 200 ms buffer trims the
excess (PROTOCOL §5.5), which is far more often the sink's pacing than the
bridge's scheduling. Attribute it before reporting it — one `WARN` per call
(`server streaming outbound audio faster than realtime`) is systematic and
points at the generator; drops scattered across a subset of calls point at
the bridge. See §8's pacing traps: `ws_sink.mjs` under-runs at low
connection counts and `paced_sink.mjs` over-runs at high ones, and a run at
200 concurrent has been measured tripping the latter on every call.

**Why 200 ms and not a rounder 250.** `SDP_NEGOTIATE_BUCKETS`
(`crates/telemetry/src/metrics.rs`) tops out at a finite `0.2` — above that
the only bucket is `+Inf`. A threshold of 250 ms would sit in the blind
spot where the histogram can no longer say *how far* over you are, so the
criterion could be satisfied but never falsified. 200 ms is the highest
real bucket edge, which makes it the highest threshold this metric can
actually adjudicate. If a 250 ms bar is ever genuinely wanted, add `0.25`
and `0.5` to the bucket list first and document the new series in
`docs/DEPLOY.md` — don't just change the number here.

**Record the knee** as *"N concurrent calls, 4 vCPU, G.711, no recording,
MOS p50 x.xx, CPU y%"*. That sentence is the deliverable.

---

## 5. Phase 2 — arrival-rate ceiling (calls per second)

**Goal:** separate *how many calls it holds* from *how fast it can set them
up*. These fail differently and both get asked about.

Hold concurrency at **50% of the knee**, then push setup rate: 10 → 25 → 50
→ 75 cps, one minute each.

**Pass:** no 5xx, no SIP retransmissions, `sdp_negotiate_seconds` p95 ≤
200 ms (§4's note explains the bound), and
`invite_admission_total{rate_limited}` == 0. **Report the cps at which p95
setup latency first exceeds 200 ms** — that's the honest number, not the
point where calls start failing.

If the ramp finishes without ever crossing it, say so in exactly those
terms: *"p95 stayed at X ms through 75 cps"*. That is a floor on the setup
rate, not a measured ceiling, and the two must not be published as if they
were the same claim.

---

## 6. Phase 3 — soak, leak audit, and graceful degradation

Three separate questions. Run them in this order.

### 6.1 Sustained soak — 60 min at 80% of the knee

**Pass:** fd count flat; thread count flat; `dialogs_active` returns to 0
after the grace window; zero unexplained call terminations
(`calls_total{cause}` should show only your own teardowns); quality SLOs
from §4 still met at minute 59.

**RSS is deliberately not a criterion here** — put it through §6.3 instead.
A soak is the *worst* place to judge memory: growth arrives in bursts
(measured: flat for eight minutes, then +4 MB in sixty seconds, then flat
again, with no correlate in CPU or load), so whether an hour looks flat or
looks like +37 MB depends on how many bursts happened to land in it. Two
matched 15-minute runs of identical work differed by 14 MB on nothing but
timing. Record the RSS curve, then interpret it with §6.3's reuse test.

### 6.2 Long-call soak — 1 call, 1 hour (`long_call_1h.xml`)

**Pass:** audio still flowing at minute 59; RSS within ±10 MB; MOS
unchanged from minute 1 (the *minimum* across the hour is the honest
statistic — an average hides a bad minute); and no **accumulating** clock
drift.

**State the drift criterion as a rate, not an absolute.** "Within a few ms
over the hour" is tighter than a userspace generator can resolve: stamping
arrival times once per 20 ms drain iteration puts ±20 ms of noise on every
sample. What distinguishes a healthy clock from a broken one is not the size
of the offset but whether it *grows*: plot RTP timestamp advance against wall
clock every 30 s and require the residual to stay in a band rather than trend.
Measured on 0.48.13: −99.9 ms over 3,599.9 s (**28 ppm**, inside crystal
tolerance) wandering in an ~80 ms band with no trend.

> **Pace the WS sink against a monotonic origin, or you will measure the
> sink.** `setInterval(…, 20)` in Node fires at *at least* 20 ms, so the sink
> feeds slower than realtime and the daemon can only play out what it is
> given. That alone produced an apparent **−0.62% daemon clock error** that
> vanished (to −0.044%) when the sink was corrected to
> `target = t0 + n × 20 ms`. It also cost the §6.1 soak 0.7% of its expected
> tx packet count, which read as the bridge dropping frames. See §8.

### 6.3 Leak audit — after every phase above

This is where resource bugs actually surface. After the load stops and all
calls have ended:

```sh
curl -s :9091/metrics | grep -E 'calls_active|conferences_active|parked_calls_active'   # all 0
curl -s :9091/metrics | grep dialogs_active    # 0, but only after the 32s grace window
ss -lnu | grep -c ':4[0-9]{4}'      # RTP ports released
ls /proc/<pid>/fd | wc -l           # back to the idle count from §3
ps -o rss= -p <pid>                 # within ~10% of pre-load idle
```

**`dialogs_active` needs the grace window and a two-step read.** Removal is
deferred by `DialogReaper::DEFAULT_GRACE` (32 s = SIP Timer H/J) so a
retransmitted BYE still matches, and the gauge is only republished on the
reaper's 5 s sweep — so a read taken immediately after teardown returns the
*pre-BYE* value. Against an outbound run that is `0`, because the UAC inserts
the dialog when it sends the BYE: assert `== 0` straight away and the audit
goes green against a daemon that reclaims nothing. Wait for the gauge to
publish non-zero, *then* wait for it to return to 0. This gauge was in §6.1's
pass criteria and missing from this list, which is part of why #548 survived
every audit run before §12 existed.

**Pass:** every counter returns to its idle value. A gauge that doesn't come
back down is a leak, and it matters more than the peak number.

**RSS is the exception, and "within ~10% of idle" is the wrong test for
it.** Two things keep it high, and neither is a leak. The daemon sizes its
buffer pools to the highest concurrency it has ever seen and keeps them —
the necessary price of allocating nothing in the frame loop (CLAUDE.md
§4.3). On top of that, sustained per-frame churn (~10,000 alloc/free pairs
per second at 200 calls) ratchets glibc's arenas upward, and glibc does not
trim them back: measured on 0.48.13, a drained-to-idle daemon held 90.2 MB
of which 76.7 MB was `RssAnon` in two mmap'd secondary arenas, and it stayed
there for ten hours. RSS therefore *never* returns to idle after load, so
that criterion fails on every run that did any work, while a real leak hides
inside the failure. Measured at 200 concurrent: **0.43 MB per call** at two
hours and still creeping, bounded and reused (test 1 below).

Use these three instead, which separate the pool from an actual leak:

1. **Re-load the same process** (start here — it is the decisive one). Take a
   daemon that has already been through a full load and drained to idle, and
   put an *identical* load through it again. Free-but-untrimmed arena gets
   handed straight back to the new calls; a leak cannot be, so it must
   allocate on top. Measured on 0.48.13: a daemon idling at 90.2 MB after
   200 calls × 60 min absorbed a second 200-call load for **+2.2 MB**, where
   a fresh daemon needed ~45 MB for the same work. That is a bounded,
   reused working set — and it is a fifteen-minute test.
2. **Repeat the peak step.** Run the same concurrency twice and require RSS
   not to grow materially the second time. Pool sizing is already paid;
   growth here is not. Make it thousands of calls, not hundreds — at a few
   KB/call, 250 calls moves RSS ~1.5 MB and is easily dismissed as noise.
3. **Vary calls independently of concurrency.** Run many short calls at
   *low* concurrency (§5's rate steps do this naturally). Any growth there
   cannot be pool sizing, because concurrency never exceeded the existing
   high-water mark. Divide by completed calls to get bytes-per-call.

**Do not infer the hot path from whether the soak's RSS looked flat.** The
tempting reading — tens of millions of frames through a couple of hundred
calls, so flat RSS means a clean frame loop — does not hold in either
direction. Growth is bursty arena expansion, so a flat hour can hide churn
and a +37 MB hour can be a daemon whose live set never moved. Test 1 is what
answers the question.

**Report the long-run figure, and do not assume it has converged.** Per-call
memory climbs for *hours* at a fixed concurrency. Measured at 200 concurrent:
~0.2 MB/call plus ~5 MB fixed at fifteen minutes, 0.40 MB/call at one hour,
**0.43 MB/call at two** — hour 2 still added 6 MB after hour 1 added 42. The
deceleration is sharp and the memory is reusable, but nothing has been
observed actually stopping. Five-minute ramp steps understate the two-hour
figure by roughly half. Quote the duration alongside the number, and if you
need a hard capacity figure, measure over the duration you intend to run.

### 6.4 Degradation past the ceiling

**How it fails is more important than where.** Set
`[sip.admission].max_concurrent` to the knee from §4, then drive **150% of
it**.

**Pass:** excess INVITEs are answered **`503`** (counted on
`invite_admission_total{result="rate_limited"}`), *admitted* calls keep
their §4 quality SLOs, and nothing degrades globally. A bridge that sheds
cleanly at a known cap is publishable; one that browns out for everyone is
not.

---

## 7. What to publish

### 7.1 Headline table

| Metric | Result |
|---|---|
| Hardware | 4 vCPU / N GB, OS/kernel |
| Version | `siphon-ai --version` |
| Codec / posture | G.711 µ-law, no recording, no HEP, energy VAD |
| **Sustained concurrent calls** | N (quality SLOs held 60 min) |
| **Setup rate** | N cps at p95 ≤ 200 ms |
| CPU at sustained load | x% of 4 cores |
| RSS at sustained load | N MB (flat over 60 min) |
| MOS p50 / loss p95 / jitter p95 | x.xx / x.xxx / x ms |
| Behaviour past the cap | 503 shed, admitted calls unaffected |

### 7.2 Feature cost deltas

Rerun §4's ramp with one feature on at a time and publish the *cost*, not
just the peak. This is more useful to an integrator than the headline, and
it is the part nobody else publishes:

| Feature | Expected pressure | Report as |
|---|---|---|
| `[recording] mode="always"` | disk I/O + a writer task per call | knee drops from N to M (−x%) |
| `[recording.storage]` upload | teardown-time spool + background upload | effect on teardown latency |
| `[hep] enabled` | UDP fan-out per SIP/RTCP event | knee delta; ~~watch HEP drop metrics~~ — **no `siphon_ai_hep_*` metric exists (#460)** |
| `[route.media] vad="neural"` | Silero inference per frame | knee delta — expect this to be the most expensive |
| Opus instead of G.711 | encode/decode per frame | knee delta |

**A full re-ramp per feature is not required.** Six steps × five features is
~2.5 hours to produce a delta that one fixed point already gives. Run every
variant at a single reference point — **80% of the baseline knee**, which §4
already has a clean row for — restart the daemon between variants, and change
one knob at a time. Anything that moves the knee moves CPU-per-call there
first. Reserve the full ramp for a feature whose cost turns out to be
non-linear in concurrency.

**Measure the marginal cost away from saturation.** At 80% of the knee an
expensive feature is contending for CPU, which *understates* its per-call cost
as fixed overhead amortises and *overstates* its latency impact. Neural VAD
measured +1.03 %/core per call at 200 concurrent but +1.47 at 25, and its
first-audio p95 was 198 ms at 200 against 18 ms at 25 — same build, same
config. Publish the low-concurrency number as the cost and the
high-concurrency number as the capacity consequence; they are different
claims.

**A loopback generator cannot exercise the HEP RTCP path.** SIPp's `rtpstream`
sends no RTCP, so `[hep] enabled` ships SIP, log and CDR chunks only —
≈7 packets per call instead of the per-RTCP-event fan-out this row is meant to
price. Do not publish a HEP cost as if it covered RTCP until §10's tier 2 runs
it with a real endpoint.

### 7.3 Publish the limits too

State `rtp_port_range` alongside the concurrency number. "250 concurrent"
without "and the port range caps you at 250" is a number that will embarrass
us the first time someone hits the wall and reports it as a bug.

---

## 8. Known traps (learned the hard way)

- **A WS sink paced with `setInterval` under-runs realtime, and the daemon
  inherits it.** Node timers fire at *at least* their interval, so a 20 ms
  `setInterval` feeds fewer than 50 frames/sec; the bridge plays out what it
  is given, so the deficit appears in *its* numbers. Measured cost: a −0.62%
  apparent clock-rate error (gone at −0.044% once the sink corrected against
  a monotonic origin) and 0.7% of the soak's expected tx packet count, which
  read as the bridge dropping frames. Pace with
  `target = t0 + n × 20 ms; setTimeout(tick, target - now)`. **No tx-rate or
  timing number measured through an uncorrected sink is worth quoting** — CPU
  and RSS work is unaffected.
- **…and the monotonic correction that fixes it *over*-runs at high
  connection counts.** `paced_sink.mjs` sends to every connection inside one
  tick. At 200 connections that loop takes longer than 20 ms, and because the
  correction targets `t0 + n × 20 ms` it then fires immediately and catches up
  in a **burst** — which the daemon correctly reads as faster-than-realtime and
  trims to its 200 ms buffer (PROTOCOL §5.5). Measured at 200 concurrent:
  69,155 `outbound_audio_frames_dropped_total` and exactly **one `WARN` per
  call**, which is the signature — a per-call count that equals the call count
  is systematic, i.e. the generator, not degradation. The two traps are mirror
  images: `setInterval` under-runs when idle, monotonic correction over-runs
  when saturated. **§1.4's "prove the sink is not the constraint" is therefore
  not satisfied at 200 by a single-process sink** — shard it across processes
  or pace per connection before quoting any aggregate outbound-path number at
  that concurrency. A canary on a *separate* WS server (§11) is unaffected.
- **Check the daemon actually started.** `Address in use` from a leftover
  instance produces failures that look exactly like load failures.
- **`gh`-style exit codes**: `cmd | tail; echo $?` reports *tail's* status.
  Check exit codes without a pipe.
- **Python buffers stdout to files** — `tool.py > out &` shows nothing until
  it exits. Use `python3 -u` when tailing a running test.
- **Prod ships `[recording] mode = "always"`.** If you load-test against a
  production-shaped config you are measuring the recorder's disk I/O.
- **HEP `queue_capacity` defaults to 256.** Under load it drops by design —
  but you cannot see it happen. `siphon_ai_hep_*` is documented in five places
  and exported nowhere (#460), and a dead collector produces no metric, no log
  line and no `/ready` change. A HEP-on run currently has no drop-rate line
  available to it; say so rather than reporting zero drops.
- **Re-sample a gauge before calling it stale.** Sampling in the same breath
  as the event that clears it produces phantom leaks.
- **`ps -o %cpu` is a lifetime average, not an instantaneous rate.** It made a
  flat 50-call run look like it was ramping (18 → 24 → 26%), and it is
  meaningless for a generator process that has been up for hours. Measure with
  `/proc/<pid>/stat` fields 14+15 delta over a fixed window. `bc` may not be
  installed on a stock box — do the arithmetic in `awk`.
- **Forcing SIGTERM during the daemon's 30 s drain orphans the calls** — no BYE,
  no CDR, and the far end holds its channels up until *its* media timeout. Wait
  the drain out, or hang up from the generator first.
- **Bound the calls, don't rely on a closing `hupall`.** Originating with
  `execute_on_answer='sched_hangup +<hold> NORMAL_CLEARING'` makes every call
  end on its own schedule, so a run that loses its driving session still
  terminates. Tier 2 (§10.2) uses this throughout.
- **Arm a detached auto-removal alongside `netem`.** It impairs your own ssh to
  the box; a dropped session must not strand the qdisc.
- **`fs_cli -x "show channels count"` prints a leading blank line** — `head -1`
  silently yields nothing. Use `tail -1`.

---

## 9. Out of scope for this pass

State these explicitly when publishing, so the numbers aren't read as
claims we haven't tested:

- **Multi-node / horizontal scaling** — untested; the architecture forbids
  shared call state, but that isn't the same as measured.
- **TLS + SRTP under load** — §§3–6 are plaintext loopback. §10 tier 2
  closes this; don't quote a secure-trunk number until it's run.
- **Mid-call WS reconnect** — post-v1, and not exercised here.
- **Sustained multi-hour behaviour** beyond the 1-hour soak. (Partly closed
  by `RESULTS-convergence-8h.md`.)
- **Outbound origination under load** was untested until §12 — every phase in
  §§3–6 and every tier in §10 drives calls *into* the daemon, including the
  `originate` commands in §10.2, which are FreeSWITCH originating **at** us.
  Nothing exercised `POST /admin/v1/calls`, so no leak audit could see a
  resource that only an originated leg allocates. Issue #548 — one dialog
  leaked per originated call, for the life of the process — survived every
  run in this document, including the 8-hour convergence soak, and was found
  by hand during a deploy verification instead. §12 closes the gap; treat the
  §§3–6 numbers as *inbound* numbers until an outbound run says otherwise.

---

## 10. Beyond loopback — a three-tier ladder

§§3–6 run SIPp against the daemon on one box, over `127.0.0.1`, in
plaintext. That is the right way to find a ceiling — free, unlimited,
deterministic, repeatable — but it leaves three things unmeasured that a
real deployment always has: **a network**, **crypto**, and **a real SIP
stack on the far end**.

Each tier below costs more than the one above it and answers a question the
one above it cannot. Run them in order; do not skip to tier 3.

| Tier | Generator | Cost | Answers |
|---|---|---|---|
| 1 | SIPp, same box, loopback | free | **Where is the ceiling?** |
| 2 | FreeSWITCH, separate box, TLS+SRTP | free | **What do the network and crypto cost?** |
| 3 | Twilio, live trunk | £/$ per minute | **Does a real carrier agree?** |

### 10.1 The generator must not live on the box under test

Tier 1 measures the daemon *and* SIPp on the same 4 vCPU. Above a few
hundred calls SIPp's own RTP work competes for the cores you are trying to
measure, and the knee you find is the pair's knee, not the daemon's.

Moving the generator to a second box fixes three things at once: the
measurement stops being contended, real UDP crosses a NIC and the kernel
network stack (so `net.core.rmem_max` starts to matter), and the far end
becomes something other than the same process family.

**Rule, same as §1.4:** whatever generates load must be proven *not* to be
the constraint. Record the generator box's CPU in every row. If it is above
~70% while the daemon under test is below it, the run is void.

### 10.2 Tier 2 — FreeSWITCH as the load generator

> **Run 2026-08-17 — see `RESULTS-tier2.md`, rig in `tier2/`.** All three phases
> (plaintext ramp, TLS+SRTP, `netem`) are done at 50/100/200 concurrent. Two
> things below need correcting in light of it: the daemon side needs
> `[media].srtp = "required"` — stock FreeSWITCH `488`s the `"preferred"`
> `RTP/AVP` + `a=crypto` shape — and FreeSWITCH's stock `max-sessions` /
> `sessions-per-second` already cover 200 at 10 cps, so the raise below is only
> needed past ~500 (~400 with crypto, where FS's own CPU starts to threaten
> §10.1).

This is the highest-value addition and it costs nothing but a second VM.
FreeSWITCH is a real SIP stack, so unlike a SIPp scenario it exercises
genuine SDP negotiation, real re-INVITE and dialog behaviour, and real RTP
timing — and it can hold hundreds of concurrent calls while playing actual
media.

**Topology**

```
┌────────────────────┐        SIP/TLS + SRTP        ┌────────────────────┐
│  Box B  (generator)│ ───────────────────────────► │  Box A  (under test)│
│  FreeSWITCH        │        real RTP over LAN     │  siphon-ai 4 vCPU  │
└────────────────────┘                              └──────────┬─────────┘
                                                               │ WS
                                                    ┌──────────▼─────────┐
                                                    │ Box C: WS sink     │
                                                    └────────────────────┘
```

Put the WS server on Box C (or at least off Box A) for the same reason —
§1.4 applies to every tier.

**Raise FreeSWITCH's own limits first.** FS will throttle before the bridge
does and you will publish FS's ceiling by mistake — the tier-2 version of
the §1.4 trap. In `autoload_configs/switch.conf.xml`:

```xml
<param name="max-sessions"        value="3000"/>   <!-- default 1000 -->
<param name="sessions-per-second" value="200"/>    <!-- default 30   -->
```

And FS has the **same two-ports-per-call arithmetic** as §1.1 — size its
RTP range to at least `2 × target_concurrency`:

```xml
<param name="rtp-start-port" value="20000"/>
<param name="rtp-end-port"   value="24000"/>       <!-- 2000 calls -->
```

**Driving the calls.** Use `bgapi` so originates don't serialise, pace the
loop to control cps, and use `endless_playback` so media keeps flowing for
the whole hold (a plain `playback` ends with the file and then the §1.3
inactivity watchdog reaps the call):

```sh
#!/usr/bin/env bash
# ramp.sh <concurrency> <cps> <hold_seconds>
CONC=${1:-100}; CPS=${2:-10}; HOLD=${3:-300}
TARGET="sip:9000@BOX_A_IP:5060"          # or ;transport=tls for the secure run
MEDIA="/usr/share/freeswitch/sounds/tone.wav"

for i in $(seq 1 "$CONC"); do
  fs_cli -x "bgapi originate {ignore_early_media=true,\
origination_caller_id_number=1000}sofia/external/$TARGET \
&endless_playback($MEDIA)" >/dev/null
  sleep "$(python3 -c "print(1/$CPS)")"
done

sleep "$HOLD"
fs_cli -x "hupall NORMAL_CLEARING"        # bounded teardown, then run §6.3
```

Watch on Box B — if either climbs, FS is the constraint:

```sh
fs_cli -x "show channels count"      # should equal your target concurrency
fs_cli -x "status"                   # session count / peak / rate
```

**What tier 2 adds over tier 1**

- **TLS + SRTP cost.** Point the FS gateway at `;transport=tls` with SRTP
  required and rerun the §4 ramp. Publish the delta — this is the number
  §9 says we must not quote without measuring, and it is the posture every
  real trunk uses.
- **Real network impairment.** Add loss/jitter/latency on Box B and watch
  the daemon's own quality metrics respond, which is also a live test of
  whether those metrics are trustworthy:

  ```sh
  sudo tc qdisc add dev eth0 root netem delay 20ms 5ms loss 0.5%
  sudo tc qdisc del dev eth0 root          # remove afterwards
  ```

  Reproducible impairment is something a real carrier can never give you.
- **A real stack on the far end**, so codec negotiation, re-INVITEs and
  RTCP come from an implementation that isn't ours.

**Pass criteria:** the §4 SLOs, plus the honest expectation that MOS will
be *below* the loopback figure. Quote the impaired MOS, not the 4.4 that
loopback produces — it is the credible number.

### 10.3 Tier 3 — a live Twilio validation run

**Twilio cannot find your ceiling, and should not be asked to.** Two
reasons:

1. Elastic SIP Trunking has per-account concurrent-channel and CPS caps.
   A ramp will be shedding on their side while the box idles — you would be
   measuring and publishing *their* throttle.
2. Sudden high-volume origination is the signature of toll fraud. **Open a
   support ticket before running anything**, say it is a scheduled load
   test, and get your actual channel/CPS limits in writing. Losing the trunk
   costs far more than this test is worth.

What a carrier uniquely adds is a **real SBC and the public internet**.
Both are per-call constants, not scaling properties — which is why a small
run is enough.

**The run:** 15 minutes at **30–50 concurrent**, after tiers 1 and 2. Only
capture the deltas: per-call CPU against the tier-2 TLS+SRTP figure, MOS
distribution against the netem figure, and carrier setup latency.

🪤 **Setup latency is not `sdp_negotiate_seconds`.** That histogram times
`prepare_call` — negotiate, port alloc, tap attach — which is *our* local
work and says nothing about the carrier. On an outbound leg the number you
want is INVITE → 200 OK, which is the CDR's `answered_at − started_at`, or
the gap between the `outbound_initiated` and `outbound_answered` webhooks.
Earlier revisions of this section named the wrong metric.

**Your concurrency limit may make the run above impossible, and the test is
still worth running.** Elastic SIP Trunking's concurrent-channel cap depends
on the account's Customer Profile: an approved *Business* profile is
effectively unlimited, but *Individual* is **3**, unapproved is 2, and trial
is 4. Below ~30 channels, run it **sequentially** instead — see §10.3.1. What
tier 3 uniquely supplies is per-call constants, and those come from *many
calls*, not from *simultaneous* ones. A sequential run actually yields a
better latency distribution: 60 samples spread over half an hour, rather than
60 set up inside one 60-second window.

### 10.3.1 The sequential (low-channel) form

Rig in `tier3/`. Each call is a **trombone**: originate through the carrier
gateway to a DID that routes back to this node, so the call returns as a
second inbound leg and real media flows both ways with no human at either
end and no AI spend.

```
siphon-ai ──INVITE──► carrier SBC ──► carrier routes the DID back
    ▲                                          │
    └────────────── inbound leg ◄──────────────┘
```

**A trombone occupies two channels**, one out and one back in. On a
3-channel trunk that means exactly one call at a time, with one channel
left free — deliberately, so a genuine inbound call is not rejected because
a test is running. Do not parallelise it without recounting channels.

What that costs you in the published claim is honesty about concurrency:

> *Confirmed against a live carrier trunk, N sequential calls: setup p50/p95
> X/Y ms, MOS p50 z.zz, packet loss L%.*

No concurrency statement at all — §10.4's template assumes the 50-concurrent
form. Tier 2 already carries concurrency scaling, so little is lost, but do
not let the sentence imply a carrier concurrency number that was never
measured.

**Cost model:** `concurrent × minutes × per-minute rate × legs`. Both legs
count if you trombone through Twilio to reach your own DID. The shape
matters more than the rate: a 15-minute run at 50 concurrent is ~750
call-minutes — tens of dollars. A 1-hour soak at 200 concurrent is 12,000+
call-minutes — hundreds. **Run the ramp, skip the soak.** Tier 2 already
covers soak for free.

**Two operational notes:** the trunk is IP-ACL authorised on a single
source IP, so this shares an authorisation path with production traffic —
run it when prod can tolerate noise. And Twilio's own concurrency cap
should be recorded next to the result, so nobody reads a Twilio number as
the daemon's ceiling.

### 10.4 The claim these three tiers earn

```
N concurrent calls on 4 vCPU (SIPp loopback, G.711, plaintext, no recording).
Validated from a separate FreeSWITCH box over TLS + SRTP at M concurrent:
+x% CPU per call, MOS p50 y.yy under 20 ms / 0.5% netem impairment.
Confirmed against a live Twilio trunk at 50 concurrent: setup p95 z ms.
RTP port range caps concurrency at range/2 — size it for your target.
```

If tier 3 ran in the §10.3.1 sequential form, its line becomes
*"confirmed against a live trunk, N sequential calls: setup p95 z ms"* —
and the claim carries no carrier-concurrency number, because none was
measured.

That is more credible than a single large loopback number, precisely
because it shows the difference between the three is understood.

---

## 11. The reference call — a human canary at full load

Stage the load from FreeSWITCH (§10.2), then place **one more call from a
real phone over the Twilio trunk** and use it normally while the box is
saturated.

Everything in §§3–6 is an aggregate: p50 and p95 across every call. None of
it answers the question a customer actually asks — *does the 501st caller
notice?* A percentile can sit inside SLO while individual calls stutter, and
a human on a live call detects one-way audio, choppiness, delay and a
sluggish bot in seconds. This is also, by a wide margin, the cheapest test
in this document: a few minutes of one PSTN call, against the hundreds of
dollars a full carrier ramp would cost.

It is a **canary, not a benchmark**. The 500 are the load; call 501 is the
probe.

### 11.1 Why it earns its place

- **It measures the experience, not the distribution.** The only test here
  whose output is "a person could not tell the box was busy."
- **It audits the metrics.** If the CDR for call 501 reports MOS 4.2 while
  the recording is audibly choppy, the quality estimator is wrong — a
  finding worth more than the capacity number, since observability is one
  of this project's main claims. The converse is equally useful: MOS 3.6
  that sounds fine tells you the estimator is conservative.
- **It exercises a second path under load.** The 500 arrive over the LAN in
  plaintext; call 501 arrives over the public PSTN with TLS + SRTP through
  a real carrier SBC. Neither §10.2 nor §10.3 covers that combination.
- **It produces the demo asset.** A clean recording of a real call made
  while 500 others were active is more persuasive to a sceptical reader
  than any table in §7.

### 11.2 Make it objective, not "sounded fine"

The failure mode of this test is a subjective verdict nobody can check.
Three things fix that.

**Record call 501 — and only call 501.** Recording all 500 turns the run
into a disk-I/O benchmark (§7.2). Use a route override keyed on your
mobile number, with global recording off:

```toml
[recording]
mode = "off"
dir  = "/var/lib/siphon-ai/recordings"

[[route]]
name = "reference-call"
[route.match]
from_user = "+1XXXXXXXXXX"        # your handset
[route.recording]
mode = "always"                    # strict override — verified, RC-03
```

**Speak a fixed script** so dropouts are detectable afterwards rather than
remembered. Counting `one … twenty` at a steady pace works: every gap,
clip, or repeat is visible in the waveform and countable.

**Turn barge-in off on the reference route, or you will measure the
barge-in policy instead of the bridge.** If the WS server echoes the caller
back — the obvious way to let a human judge the return path — then under
`[bridge.barge_in] mode = "pause"` (or `auto_clear`) every spoken word
suppresses playout of *its own echo*, and the caller hears exactly what a
degraded call sounds like on a completely idle box. Measured: 44% echo
retention under `pause` against 89% with it off, `barge_in_count` 20 for a
count to twenty, and `tx_packets_sent` ~27% below the continuous-50 fps
figure. The inbound half was pristine throughout, so no metric flags it —
only the caller does.

The artifact would appear in both halves of the A/B, so the comparison
still technically holds; the reason to remove it anyway is that it makes a
load-induced dropout indistinguishable from barge-in cutting the caller's
own voice, which is the one signal this test exists to detect. Use a
per-route override so production policy is untouched:

```toml
[route.bridge.barge_in]
mode = "notify_only"               # VAD still reported; playout never cut
```

It is route-level, so `systemctl reload` applies it — no restart, no
dropped calls. `notify_only` is the right mode rather than deleting the
block: speech events keep flowing to the WS server, so the observability
half of the call is unchanged.

**Run it as an A/B.** Place the identical call with the load **off**, then
again at full load, and compare:

| Compare | Idle | Loaded |
|---|---|---|
| CDR `quality.first_audio_out_ms` | | |
| CDR `quality.avg_jitter_ms` / `max_jitter_ms` | | |
| CDR `quality.avg_packet_loss_ratio` | | |
| CDR `quality.mos_estimate_min` / `_avg` | | |
| `sdp_negotiate_seconds` for this call | | |
| Recording: dropouts, clipped words, silence gaps | | |
| Subjective: 1–5, and *what* was wrong | | |

`first_audio_out_ms` is the one to watch hardest — for a voice-AI bridge
it is the "does the bot answer instantly" number, and it is the first thing
that degrades under scheduler pressure.

### 11.3 The ladder version

If time allows, place the reference call at **0 / 100 / 250 / 400 / 500**
concurrent with the same spoken script each time. That produces a
perceptual curve alongside the metric curve, and lets you say where quality
*starts* to degrade rather than only where it breaks. Very little
telephony infrastructure publishes anything like it.

### 11.4 Traps specific to this test

- **Size the RTP range for 501, not 500.** With `[40000, 41002]` (501
  pairs) exactly, the 500 staged calls leave one pair and any teardown
  churn takes it. If your own call is rejected, that is a §1.1 port result,
  not a quality result. Give it real headroom.
- **Admission control will reject the human too.** If §6.4's
  `max_concurrent` is still set from the degradation test, call 501 is the
  one that gets the 503. Clear it first — or set it *deliberately* to 500
  and confirm your call is refused cleanly, which is its own useful result.
- **Don't leave prod's `mode = "always"` on.** Recording 500 concurrent
  calls is a different experiment.
- **Keep the WS server honest.** Whatever answers call 501 must not be the
  same saturated Python echo server carrying the other 500 (§1.4), or you
  are hearing its scheduling, not the bridge's.
- **Capture the call_id.** Note it from `GET /admin/v1/calls` while the call
  is up, so the CDR, the recording and the HEP chunks can all be pulled for
  that exact call afterwards.

### 11.5 Pass criteria

The reference call at full load holds every §4 SLO **and** is subjectively
indistinguishable from the idle A/B — no audible dropouts, no one-way
audio, no added delay, and `first_audio_out_ms` within ~20% of idle.

If it passes, the honest headline is not a number at all:

> *A live PSTN call placed while 500 concurrent calls were active was
> indistinguishable from an idle one — same MOS, same first-audio latency,
> no audible degradation.*

---

## 12. Outbound origination under load

Everything above drives calls **into** the daemon. This phase drives them
**out** of it, through `POST /admin/v1/calls`.

The distinction is not cosmetic. An originated leg runs a different
teardown path, a different dialog lifecycle, and a different admission
gate (`[outbound].max_concurrent` / `rate_limit_per_sec`) from an accepted
one, and none of it had ever been under load. The cost of that showed up as
**issue #548**: one dialog leaked into the shared store per originated call,
for the life of the process, until `sip-dialog`'s `MAX_CONFIRMED_DIALOGS`
(10,000) is reached and in-dialog requests silently stop matching — for
inbound calls too, since the store is shared. It survived every phase in
this document, the 8-hour convergence soak included, and was found by hand
while verifying a deploy. A leak audit cannot see a resource no phase
allocates.

Read the §§3–6 figures as **inbound** figures. This phase is what lets you
say anything about the other direction.

### 12.1 The rig

`outbound/` in this directory. SIPp is the callee, the daemon is the caller,
and `paced_sink.mjs` holds the WS side up for the duration:

```
ob_ramp.sh ──POST /admin/v1/calls──► siphon-ai ──INVITE──► SIPp (uas_hold.xml)
                                         │
                                         └──WS──► paced_sink.mjs
```

| | what it is |
|---|---|
| `phase-outbound.sh` | the driver: steps, sampling, drain, leak audit |
| `ob_ramp.sh` | originate N at C cps, hold, tear down, wait for drain |
| `obstat.sh` | one-shot JSON sample (CPU as a *rate*, RSS, fds, the gauges) |
| `uas_hold.xml` | SIPp answers and holds until **we** BYE |
| `uas_hold_remote_bye.xml` | SIPp answers, holds, then **it** BYEs |
| `outbound-lab.toml.example` | daemon config — ports clear of a prod install |

```sh
cd test-harness/load/outbound
cp outbound-lab.toml.example outbound-lab.toml
export SP=/var/tmp/outbound-load
HOLD=120 CPS=5 ./phase-outbound.sh /usr/bin/siphon-ai run-label 25 50 100
TEARDOWN=remote ./phase-outbound.sh /usr/bin/siphon-ai run-label-remote 50
```

### 12.2 Run both teardown directions

Who hangs up decides which branch of the teardown executes, and #548 sat on
one of them. `uas_hold.xml` has the driver hang up through the admin API, so
**we** send the BYE; `uas_hold_remote_bye.xml` has SIPp send it. A fix that
retires the dialog in only one branch passes half this phase — which is why
the phase runs both rather than picking the convenient one.

### 12.3 What to measure

Per step, the §§3–6 quantities plus the two the originate path adds:

- `siphon_ai_outbound_calls_total{result}` — everything should be
  `answered`. A `rejected`/`unreachable` count is the far end or the trunk,
  not the daemon, and invalidates the step's per-call arithmetic.
- **Originates the daemon refused.** `ob_ramp.sh` counts non-202 responses
  and says so. A `503` is `max_concurrent`, a `429` is `rate_limit_per_sec`
  — both are the daemon's own guardrails, and a step that silently ran at
  half its target because of them is worse than no step at all.

### 12.4 Pass criteria

Everything in §6.3, run **with `dialogs_active` in it** and read the
two-step way that section describes. Plus:

- Every originate returns `202`, and every call ends `answered`.
- `dialogs_active` returns to 0 after the grace window at **every** step, in
  **both** teardown directions. This is the assertion #548 would have failed.
- RTP ports released — an originated leg holds two, exactly like an accepted
  one, so §1.1's ceiling is a ceiling on the *sum* of both directions.

### 12.5 Traps

- **The inactivity watchdog reaps originated legs too.** They are held open
  by the driver, not by media, and the sink only feeds them once the bridge
  is up. Set `inactivity_timeout_secs = 0` (§1.3).
- **Don't use the echo server's `--auto-hangup-after-ms`.** It ends the call
  from the WS side after a beat, which is right for a conformance scenario
  and fatal for a hold.
- **`[outbound]` is fail-closed**: unset `max_concurrent` means outbound is
  *disabled*, and every originate returns `501`. That reads like a broken
  rig rather than a config gap.
