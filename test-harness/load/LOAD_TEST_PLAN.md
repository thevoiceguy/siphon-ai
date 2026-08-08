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
| `[40000, 42000]` — needed for a 500-call test | 2000 | **1000** |

**The `concurrent_burst_500.xml` scenario cannot pass with the config the
README points at** — it exhausts ports at 50 calls and reports failures that
look like the bridge falling over. Size the range to at least `2 ×
target_concurrency` (plus headroom for calls in teardown, which hold their
ports briefly) before running anything in §4.

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

### 1.3 The inactivity watchdog kills silent calls

`[media].inactivity_timeout_secs` (default 60) tears down a call with no
inbound RTP. A soak that doesn't stream real audio dies at the 60-second
mark and looks like a stability bug. Either stream a pcap (preferred — it
exercises the jitter buffer and codec path, which is most of the per-call
CPU) or set `inactivity_timeout_secs = 0` and label the run
**signalling-only**.

### 1.4 The WS server is probably your bottleneck, not the bridge

This is the trap that produces a wrong headline number. The Python echo
server in `examples/echo-ws-server-python` is single-threaded and will
saturate well before the bridge does — you would be publishing *its*
ceiling.

Mitigate all three ways:

1. Use the Node echo server, or a trivial WS sink that discards audio and
   sends silence.
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
| Jitter / loss / MOS | `siphon_ai_rtp_jitter_ms`, `rtp_packet_loss_ratio`, `rtp_mos_estimate` |
| Setup latency | `siphon_ai_sdp_negotiate_seconds`, `siphon_ai_ws_connect_seconds` |

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
| Media quality | `rtp_mos_estimate` p50 ≥ 4.0, `rtp_packet_loss_ratio` p95 ≤ 0.01 |
| Jitter | `rtp_jitter_ms` p95 ≤ 30 ms |
| Playout | `siphon_ai_outbound_audio_frames_dropped_total` stays 0 |
| Setup latency | `sdp_negotiate_seconds` p95 ≤ 250 ms |
| Headroom | bridge CPU ≤ 80% of total cores; RSS not growing within the step |
| Not port-capped | concurrency < `rtp_port_range / 2` (else you measured §1.1) |

**Record the knee** as *"N concurrent calls, 4 vCPU, G.711, no recording,
MOS p50 x.xx, CPU y%"*. That sentence is the deliverable.

---

## 5. Phase 2 — arrival-rate ceiling (calls per second)

**Goal:** separate *how many calls it holds* from *how fast it can set them
up*. These fail differently and both get asked about.

Hold concurrency at **50% of the knee**, then push setup rate: 10 → 25 → 50
→ 75 cps, one minute each.

**Pass:** no 5xx, no SIP retransmissions, `sdp_negotiate_seconds` p95 ≤
250 ms, and `invite_admission_total{rate_limited}` == 0. **Report the cps at
which p95 setup latency first exceeds 250 ms** — that's the honest number,
not the point where calls start failing.

---

## 6. Phase 3 — soak, leak audit, and graceful degradation

Three separate questions. Run them in this order.

### 6.1 Sustained soak — 60 min at 80% of the knee

**Pass:** RSS flat within ±10 MB after a 5-minute warm-up; fd count flat;
thread count flat; zero unexplained call terminations
(`calls_total{cause}` should show only your own teardowns); quality SLOs
from §4 still met at minute 59.

### 6.2 Long-call soak — 1 call, 1 hour (`long_call_1h.xml`)

**Pass:** audio still flowing at minute 59; RSS within ±10 MB; no clock
drift (RTP timestamp advance matches wall clock within a few ms over the
hour); MOS unchanged from minute 1.

### 6.3 Leak audit — after every phase above

This is where resource bugs actually surface. After the load stops and all
calls have ended:

```sh
curl -s :9091/metrics | grep -E 'calls_active|conferences_active|parked_calls_active'   # all 0
ss -lnu | grep -c ':4[0-9]{4}'      # RTP ports released
ls /proc/<pid>/fd | wc -l           # back to the idle count from §3
ps -o rss= -p <pid>                 # within ~10% of pre-load idle
```

**Pass:** every counter returns to its idle value. A gauge that doesn't come
back down is a leak, and it matters more than the peak number.

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
| **Setup rate** | N cps at p95 ≤ 250 ms |
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
| `[hep] enabled` | UDP fan-out per SIP/RTCP event | knee delta; watch HEP drop metrics |
| `[route.media] vad="neural"` | Silero inference per frame | knee delta — expect this to be the most expensive |
| Opus instead of G.711 | encode/decode per frame | knee delta |

### 7.3 Publish the limits too

State `rtp_port_range` alongside the concurrency number. "250 concurrent"
without "and the port range caps you at 250" is a number that will embarrass
us the first time someone hits the wall and reports it as a bug.

---

## 8. Known traps (learned the hard way)

- **Check the daemon actually started.** `Address in use` from a leftover
  instance produces failures that look exactly like load failures.
- **`gh`-style exit codes**: `cmd | tail; echo $?` reports *tail's* status.
  Check exit codes without a pipe.
- **Python buffers stdout to files** — `tool.py > out &` shows nothing until
  it exits. Use `python3 -u` when tailing a running test.
- **Prod ships `[recording] mode = "always"`.** If you load-test against a
  production-shaped config you are measuring the recorder's disk I/O.
- **HEP `queue_capacity` defaults to 256.** Under load it drops by design;
  that's `siphon_ai_hep_*` moving, not a bug — but it does mean HEP-on runs
  need their own drop-rate line in the results.
- **Re-sample a gauge before calling it stale.** Sampling in the same breath
  as the event that clears it produces phantom leaks.

---

## 9. Out of scope for this pass

State these explicitly when publishing, so the numbers aren't read as
claims we haven't tested:

- **Multi-node / horizontal scaling** — untested; the architecture forbids
  shared call state, but that isn't the same as measured.
- **TLS + SRTP under load** — the ramp above is plaintext loopback. Expect a
  real cost; measure it before quoting a secure-trunk number.
- **Mid-call WS reconnect** — post-v1, and not exercised here.
- **Sustained multi-hour behaviour** beyond the 1-hour soak.
