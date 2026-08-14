# Load results — 0.48.18 (§6.1 soak + §6.3 leak audit)

Run 2026-08-14 against the **shipped 0.48.18 `.deb` binary** (sha256
`ecbc5e128…`, the same artifact production runs), not a dev build.

Scope: `LOAD_TEST_PLAN.md` §6.1 (60-minute sustained soak) and §6.3 (leak
audit + reuse test). The §4 concurrency ramp and §5 arrival-rate ceiling
were **not** re-run — 0.48.18 changes one branch in the ACK path and
nothing in setup or media, so the 0.48.13 figures for those stand.

## Environment

| | |
|---|---|
| Hardware | 4 vCPU, 7.9 GB RAM |
| OS / kernel | Debian 13, Linux 6.12.95+deb13-amd64 |
| Version | `siphon-ai 0.48.18` (shipped deb) |
| Posture | G.711 µ-law, recording **off**, HEP **off**, energy VAD, no webhooks, no audit |
| `[sip].udp_rate_limit_pps` | **0 (disabled)** — as in the 0.48.13 run, so §5-style measurements are of the bridge, not the transport cap (#459) |
| `rtp_port_range` | `[41000, 45000]` |
| Generator | SIPp, same box, real PCMU RTP per call |
| WS server | **`paced_sink.mjs`** on `127.0.0.1:8767` — the monotonic-origin sink, not `ws_sink.mjs` (see §8 / the trap below) |

Second unprivileged instance on `:5070` / metrics `:9191`, alongside an
untouched production daemon on `:5060`.

## Headline

| Criterion (§6.1) | Threshold | 0.48.18 | 0.48.13 |
|---|---|---|---|
| Call drops during soak | zero | **zero** — 200/200, `sipp_exit=0` | zero |
| Terminations | your own teardowns only | **200 × `caller_hangup`**, no other label | same |
| fds flat | — | **612** for 237 consecutive samples (`612 = 12 + 3×200`) | 612 |
| threads flat | — | **5** (6 momentarily during ramp) | 5 |
| Quality at minute 59 | MOS p50 ≥ 4.0, jitter p95 ≤ 30 ms | **MOS 100 % > 4.4; jitter 79.6 % ≤ 10 ms, 100 % ≤ 20 ms** | same |
| RSS flat after warm-up | ±10 MB | **+33.7 MB** (54.0 → 87.7) — see §6.3 | +36.8 MB |
| CPU at 200 concurrent | — | **105.1 %** of one core = 26.3 % of 4 vCPU | 134.5 % at 250 |
| CPU per call | — | **0.53 %/core** | 0.54 %/core |

Minute-59 quality is the **T+5 → T+59 delta alone** (129,604 jitter
samples), not cumulative, so a bad final minute cannot hide behind a good
hour. That sample count is identical to the 0.48.13 run's, which is the
cheapest confirmation the two runs did the same work.

**Media:** 35,999,837 RTP packets received against a theoretical
36,000,000 (200 × 3600 s × 50 fps) = **99.9995 %**;
`siphon_ai_outbound_audio_frames_dropped_total` **0**. The WS sink held
50.031 Hz and never exceeded 38 % of one core, so §1.4's "prove the sink
is not the constraint" is satisfied.

**The daemon logged exactly one line above `INFO` in the whole hour**, and
it is the subject of the next section.

## §6.3 — leak audit

| | pre-load | post-drain |
|---|---|---|
| fds | 12 | **12** |
| threads | 6 | 5 |
| `calls_active` / `dialogs_active` | 0 / 0 | **0 / 0** |
| RSS | 14.7 MB | 87.0 MB |

File descriptors return to the baseline **exactly**, which is the check
that actually distinguishes a leak from allocator behaviour.

**Reuse test.** A second full 200-call load on the *same* drained daemon
cost **+10.1 MB** (87.0 → 97.1), against +72 MB for the first. Growth is
front-loaded and does not repeat per unit of work, so this is retention,
not a per-call leak — the same conclusion 0.48.13 reached (+2.2 MB on its
second load). The larger figure here is not comparable like-for-like: this
re-load ran 4 minutes against 0.48.13's shorter probe, and §6.1 already
documents that RSS growth arrives in bursts, which is why **RSS is not a
§6.1 pass criterion**.

## The one failure, and it is not the daemon's fault

Call 68 of the reuse load drew a **`500 Server Internal Error`**:

```
WARN rejecting INVITE error=forge session error: Network error:
  Failed to bind socket to 0.0.0.0:44134: Address in use (os error 98)
  code=500 reason="Server Internal Error"
```

`README.md` lists a 500 here as "should NOT happen … unless forge itself
is broken". Forge is not broken. The cause is a **configuration overlap
this repo does not warn about anywhere**:

| | |
|---|---|
| `net.ipv4.ip_local_port_range` | **32768–60999** |
| this run's `rtp_port_range` | 41000–45000 — **entirely inside it** |
| **production's `rtp_port_range`** | **40000–40500 — also inside it** |

Any process on the host that opens a UDP socket without binding an
explicit port gets a random ephemeral port, and the kernel is free to hand
it one that the daemon considers part of its RTP pool. When the daemon
later tries to bind that port for a call, the bind fails and the INVITE is
rejected 500. One call in 399 hit it here.

Evidence it is an outside squatter rather than the daemon double-issuing a
port: `44134` appears **zero** times as a successfully allocated
`rtp_port` in the daemon's entire log, all 400 in-range UDP sockets during
load are accounted for as 200 calls × (RTP + RTCP), and SIPp was confined
to 6000–16000.

**What I could not establish:** which process held it. The window was
transient and the range is clear when the box is idle, so the specific
squatter is unidentified — the mechanism is proven, the culprit is not.
Note also that the daemon's own **171 WS client TCP sockets** sat inside
41000–45000 during load; they are *not* the cause (TCP and UDP port spaces
are independent) but they show how thoroughly the ranges overlap.

**Fix, for deployments as much as for this harness:** reserve the RTP
range so the kernel never issues it ephemerally —

```sh
sysctl -w net.ipv4.ip_local_reserved_ports=40000-40500   # match rtp_port_range
```

or move `rtp_port_range` above `ip_local_port_range`'s ceiling. Neither
`docs/DEPLOY.md`, `docs/CONFIG.md` nor `LOAD_TEST_PLAN.md` §1 mentions
this today, and §1.1 ("RTP port range is the real ceiling") is exactly
where an operator would look for it.

## Verdict

0.48.18 matches 0.48.13 on every §6.1 criterion, at identical per-call
CPU, with file descriptors and threads immovable across the hour and all
200 calls torn down by their own BYE. Nothing in the ACK-dispatch change
(#500) shows up under sustained load, which is what this run was for.

The one 500 is an environment overlap that predates this release and
affects production's current configuration too; it is tracked separately
from the release.

## Not measured

§4 ramp, §5 arrival-rate ceiling, §6.2 long-call (1 × 1 h), §6.4
degradation past the cap. 0.48.13's figures stand for all four.
