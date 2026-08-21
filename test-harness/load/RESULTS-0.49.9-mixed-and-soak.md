# §13 mixed direction + §12.6 outbound soak — 0.49.9

Run 2026-08-21 against the **shipped 0.49.9 `.deb`** binary
(sha256 `ca7a6af19…`). Closes the two gaps `RESULTS-0.49.9-outbound.md`
left open: what the two directions do to each other, and what the
origination path does over an hour.

**Result: pass on all three.** The directions compose linearly with no
interaction penalty; the shared RTP pool fails outbound cleanly and leaves
inbound untouched; and 3,745 originated calls over 75 minutes leaked
nothing — the RSS growth is arena, proven by re-load, not a leak.

---

## Environment

| | |
|---|---|
| Box | 4 vCPU, 7947 MB, Debian 13, Linux 6.12.95+deb13-amd64 |
| Daemon | `siphon-ai 0.49.9` (shipped deb), `127.0.0.1:5070` |
| Peers | SIPp as UAC (inbound) and as UAS (outbound), `paced_sink.mjs` on `:8770` |
| Posture | PCMU, plaintext, `[cdr.file]` on, no HEP/webhooks/recording/VAD |

---

## 1. Mixed direction — the two compose linearly

50 inbound legs established first, then 50 outbound originated into the
same daemon, so the outbound side had to find ports in a pool already half
consumed.

| | idle | 50 inbound | **50 in + 50 out** |
|---|---|---|---|
| CPU (% of one core) | 0.0 | 14.0 | **27.2** |
| RSS | 16.0 MB | 28.2 MB | 40.3 MB |
| fds | 13 | 163 | **313** |
| UDP sockets | 1 | 101 | **201** |
| `calls_active` | 0 | 50 | 100 |

🔑 **`fds = 13 + 3N` and `udp_sockets = 2N + 1` hold across the mix**, with
N the *total* of both directions. An originated leg and an accepted leg
cost the same three descriptors and the same port pair; nothing about the
combination is special.

🔑 **No interaction penalty.** 100 mixed calls cost 27.2 % of one core
against **27.1 %** for 100 outbound-only in `RESULTS-0.49.9-outbound.md`.
Within sampling noise, a mixed call costs what either kind costs alone.

Teardown was clean in both directions: every inbound leg `caller_hangup`,
every outbound leg `local_shutdown`, `dialogs_active` back to 0 at t=37 s,
fds and sockets back to idle, **zero WARN/ERROR**.

## 2. The shared RTP pool — and there is no reservation

Same rig, `rtp_port_range` deliberately shrunk to **120 ports = 60 calls
total**, then asked for 50 inbound + 20 outbound = 70.

| | result |
|---|---|
| inbound established | **50 / 50** — unaffected throughout |
| outbound answered | **10** |
| outbound failed | **10** |
| UDP sockets at peak | **121** = the whole pool + the SIP listener |
| `dialogs_active` after drain | **0** — the failed originates leaked nothing |

Each failure is explicit and correctly attributed:

```
WARN siphon_ai_core::outbound_service: outbound call did not connect
     error=forge session error: Resource limit exceeded: No available ports in pool
siphon_ai_outbound_calls_total{result="failed"} 10
```

🔑 **First-come-first-served, with no reservation between directions.**
Inbound got there first and kept every port it took; outbound absorbed the
entire shortfall. A node that both answers and originates can therefore
have its origination *completely* starved by an inbound surge, with no
inbound symptom at all — and outbound calls are usually the ones with a
business deadline attached.

🪤 **The originate API returns `202` and fails asynchronously**, so this is
invisible to a caller watching HTTP status. `ob_ramp.sh` counted
`rejected=0` for the run in which half the calls failed: its counter sees
admission refusals (`503` `max_concurrent`, `429` `rate_limit_per_sec`),
not resource failures, which surface later on the `outbound_failed`
webhook and the metric.

**What to do about it**, since the daemon is behaving correctly here:

- size `rtp_port_range` for the **sum** of both directions plus headroom,
  not for the busier one (§1.1);
- alert on `siphon_ai_outbound_calls_total{result="failed"}` — on a node
  that originates, it is the only signal this is happening;
- if origination must be protected from inbound, the lever today is
  `[sip.admission]` capping inbound, not a reservation in the pool.

## 3. Outbound soak — 60 minutes, and it churns

§6.1's soak in the outbound direction. **Not** 50 long calls: it holds 50
concurrent by *replacing* each call as it ends, so an hour at a 60 s hold
is ~3,000 completed calls. That matters — per-call leaks scale with
completions, not concurrency, and #548 leaked one dialog per call. A soak
that merely held 50 calls open for an hour would have missed it entirely.

**2,995 placed, 2,995 completed, 0 failed. Zero WARN/ERROR in the whole
75-minute session** (soak + re-load).

| | during | at drain |
|---|---|---|
| `calls_active` | exactly 50, all hour | 0 |
| `dialogs_active` | 50 → 76 | **0 at t=35 s** |
| fds | 163, flat | **13** (idle) |
| UDP sockets | 101, flat | **1** |
| CPU | 13.8 – 15.4 % | 0 |

🔑 **`dialogs_active` sits *above* concurrency under churn, and that is
correct.** 50 active plus ~26 finished-but-still-in-grace (32 s window at
~50 calls/min) = 76. §6.1's criterion — "returns to 0 after the grace
window" — only applies once load *stops*; during sustained churn the
steady-state value is `concurrency + churn_rate × grace`. Reading it as a
leak would be a false alarm, and reading a *flat* 76 as healthy is the
correct call.

### 3.1 RSS grew, and the re-load test says it is not a leak

RSS climbed 45.3 → 108.0 MB across the hour at **constant** concurrency —
~21 KB per completed call, against the ~6–10 KB/call §6.3 documents for
inbound. Pool sizing was paid in the first minute, so this is churn, and
the growth was steppy (+16.6 MB then +22.5 MB between five-minute samples)
rather than linear.

§6.3's test 1 settles it. The same process, already drained, was given an
identical load:

| | calls | RSS | per call |
|---|---|---|---|
| first load | 2,995 | 45.3 → 108.0 MB (**+62.7**) | 21.4 KB |
| **re-load, same process** | 750 | 108.0 → 109.4 MB (**+1.4**) | **1.9 KB** |

A leak must allocate on top: 750 calls at 21.4 KB would have added ~16 MB.
It added **1.4** — 91 % less per call on the second pass. Free-but-untrimmed
glibc arena is handed straight back to the new calls; a leak cannot be.

🔑 **The outbound path's memory is bounded and reused**, the same
conclusion `RESULTS-0.48.13.md` reached for inbound (a drained 90.2 MB
daemon absorbing a whole second load for +2.2 MB).

⚠️ **Do not quote 21 KB/call as a capacity number.** It is the *first-fill*
cost of a fresh process reaching its working set, not a recurring one. The
number that matters for sizing is the second: ~2 KB/call once the arena is
warm.

---

## What this does not cover

- **Mixed direction at scale.** 50 + 50; nothing says the linearity holds
  at 200 + 200, though nothing suggests it breaks either.
- **Mixed direction over time.** §13 is a steady-state snapshot; there is
  no hour-long *mixed* soak.
- **Pool exhaustion from the other side.** Inbound won the race here
  because it was established first. What an *outbound*-saturated pool does
  to arriving INVITEs (a 503? a 488? a hang?) is untested and is the
  obvious next question.
- **The carrier path.** All of this is loopback; §10.3's tier 3 covers the
  live trunk, at one call at a time.
