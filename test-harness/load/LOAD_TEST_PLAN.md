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
| Playout | ~~`siphon_ai_outbound_audio_frames_dropped_total`~~ — **no such metric; this SLO has no data source** |
| Setup latency | `sdp_negotiate_seconds` p95 ≤ 200 ms (see note) |
| Headroom | bridge CPU ≤ 80% of total cores; RSS not growing within the step |
| Not port-capped | concurrency < `rtp_port_range / 2` (else you measured §1.1) |

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

**RSS is the exception, and "within ~10% of idle" is the wrong test for
it.** The daemon sizes its buffer pools to the highest concurrency it has
ever seen and keeps them — the necessary price of allocating nothing in the
frame loop (CLAUDE.md §4.3). RSS therefore *never* returns to idle after
load, so that criterion fails on every run that did any work, while a real
leak hides inside the failure. Measured here: ~0.23 MB per call of
high-water pool, which is bounded and legitimate.

Use these two instead, which separate the pool from an actual leak:

1. **Repeat the peak step.** Run the same concurrency twice and require RSS
   not to grow materially the second time. Pool sizing is already paid;
   growth here is not. Make it thousands of calls, not hundreds — at a few
   KB/call, 250 calls moves RSS ~1.5 MB and is easily dismissed as noise.
2. **Vary calls independently of concurrency.** Run many short calls at
   *low* concurrency (§5's rate steps do this naturally). Any growth there
   cannot be pool sizing, because concurrency never exceeded the existing
   high-water mark. Divide by completed calls to get bytes-per-call.

A per-frame leak is ruled out separately by §6.2/§6.1: a 60-minute soak
puts tens of millions of frames through a couple of hundred calls, so flat
RSS across the hour means the hot path is clean whatever the per-call
figure says.

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
- **TLS + SRTP under load** — §§3–6 are plaintext loopback. §10 tier 2
  closes this; don't quote a secure-trunk number until it's run.
- **Mid-call WS reconnect** — post-v1, and not exercised here.
- **Sustained multi-hour behaviour** beyond the 1-hour soak.

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
distribution against the netem figure, and carrier setup latency
(`sdp_negotiate_seconds` p95).

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
