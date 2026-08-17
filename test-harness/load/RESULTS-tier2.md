# Tier 2 results — FreeSWITCH generator, separate box, TLS + SRTP, netem

Run 2026-08-17 against the **shipped 0.48.19 `.deb` binary** (sha256
`6b7340df4…`, byte-identical to the one the reference node runs), driven by a
real FreeSWITCH on a second host.

Scope: `LOAD_TEST_PLAN.md` **§10.2** in three phases — a plaintext concurrency
ramp over the network, the same ramp over SIP/TLS with SRTP, and a quality pass
under `netem` impairment. **1,100 calls, zero failures, and not one log line
above `INFO` from either daemon across the whole session.**

This is the tier that §9 says must exist before quoting a secure-trunk number.
It does not replace tier 1: §§4–6 remain the source for the knee, the soak and
the leak audit.

## Environment

| | Box A — under test | Box B — generator |
|---|---|---|
| Hardware | 4 vCPU, 7.9 GB | 4 vCPU, 7.9 GB |
| OS | Debian 13, Linux 6.12.95 | Debian 12 |
| Software | `siphon-ai 0.48.19` (shipped deb) on `:5070` udp / `:5071` tls | FreeSWITCH 1.11.0 `mod_sofia`, external profile |
| RTT | **0.27 ms** — same datacenter | |

Posture on Box A is copied from the tier-1 soak config so any delta is
attributable to the generator, the network and the crypto rather than to
posture drift: G.711 µ-law, recording **off**, HEP **off**, energy VAD, no
webhooks, no audit, `udp_rate_limit_pps = 0`, `rtp_port_range = [41000, 45000]`.

**WS sink:** `paced_sink.mjs` on Box A `:8767` — the monotonic-origin sink, not
`ws_sink.mjs` (§8's pacing trap). It held **50.000 Hz across 252,002 ticks** for
the entire session, so §1.4's "prove the sink is not the constraint" is
satisfied throughout.

**Generator shape.** Calls are originated with
`execute_on_answer='sched_hangup +<hold> NORMAL_CLEARING'`, so **every call
terminates on its own schedule** rather than depending on a closing `hupall`.
A run that loses its driving session still ends itself. Mean measured duration
was 299,8xx ms against a 300,000 ms target in every step.

A third box for the WS sink was not available, so the sink runs on Box A. Its
cost is inside the daemon-box figures below and is not separated out.

---

## Phase 1 — concurrency ramp, plaintext, over the network

5 minutes per step, three CPU samples per step, `/proc/<pid>/stat` deltas over
fixed windows (**not** `ps %cpu`, which is a lifetime average — see Traps).

| Concurrency | Daemon %of one core | CPU/call | FreeSWITCH %of one core | RSS | fds | threads |
|---|---|---|---|---|---|---|
| 50 | 35.7 | 0.713 % | 12.8 | 35 MB | 162 | 5 |
| 100 | 65.5 | 0.655 % | 29.8 | 51 MB | 312 | 5 |
| 200 | **129.9** (32.5 % of the box) | **0.650 %** | 54.5 | 70 MB | 612 | 5 |

**The network and a real SIP stack cost +23 % CPU per call.** Tier 1 measured
**0.53 %/call at the same 200 concurrent** (`RESULTS-0.48.18.md`) with SIPp on
loopback; this run measures **0.650 %/call**. That is the number §9 forbids
quoting without measuring.

**No knee at 200 over the network.** Per-call cost is flat-to-improving as
concurrency rises (0.713 → 0.655 → 0.650) — fixed overhead amortising, not
saturation. The ceiling remains the port range, as §1.1 says.

Three independent checks say the two tiers measured the same system:

- **File descriptors are exactly `12 + 3N` at every step**, and **612 at 200
  concurrent is the identical number tier 1 published**.
- **Threads pinned at 5** throughout, as in every tier-1 run.
- **RTP arrived at rate**: 2,670,523 packets during the 200-call step against
  ~2.5 M expected for the sampled window at 50 fps — the calls carried media,
  they were not merely signalled up.

`siphon_ai_outbound_audio_frames_dropped_total` was **0** at every step, so the
§4 Playout SLO is asserted here rather than skipped (it was unassertable before
0.48.18 — #474).

---

## Phase 2 — SIP/TLS + SRTP

Identical ramp over `;transport=tls` with `rtp_secure_media=mandatory:AES_CM_128_HMAC_SHA1_80`
and `[media].srtp = "required"` on the daemon.

> `"required"`, not `"preferred"`: stock FreeSWITCH rejects the preferred-mode
> `RTP/AVP` + `a=crypto` offer outright (`488`). Both `docs/CONFIG.md` and
> `docs/FREESWITCH_INTEGRATION.md` say so; this run is the confirmation that
> `required` is the workable mode toward FS.

| Concurrency | Daemon: plain → TLS+SRTP | CPU/call | FreeSWITCH: plain → TLS+SRTP | fds | RSS |
|---|---|---|---|---|---|
| 50 | 35.7 → 38.4 (+7.6 %) | 0.767 % | 12.8 → 23.5 | 164 | 39 MB |
| 100 | 65.5 → 71.7 (+9.4 %) | 0.717 % | 29.8 → 47.8 | 314 | 63 MB |
| 200 | 129.9 → **132.6** | **0.663 %** | 54.5 → **102.9** | 614 | 79 MB |

**Coverage is proven, not assumed.**
`forge_srtp_packets_decrypted_total` = **5,248,192** =
`forge_rtp_packets_received_total`. Every received packet was decrypted, so no
call quietly negotiated its way back to plaintext — the failure mode that would
have made this phase meaningless.

**Crypto is cheap for SiphonAI and expensive for FreeSWITCH.** The daemon pays
under 10 %, and at 200 concurrent **the difference is inside the sample spread**
— 128.6 / 140.5 / 128.8 against 127.5 / 132.6 / 129.7, medians that actually
cross. Do not quote "+2.1 % at 200" as a measured cost; the honest claim is
"under 10 %, and not separable from noise at 200". FreeSWITCH, doing the same
crypto on the same media, rose **60–89 %**.

The asymmetry is structural rather than surprising: AES-CM-128 over 20 ms G.711
frames is small next to the per-frame work the daemon already does (jitter
buffer, WS framing, VAD), and **TLS is signalling-only** — one connection whose
handshake amortises across the run, which is why file descriptors moved just
612 → 614. RSS rose ~10 MB at 200 for the SRTP contexts.

---

## Phase 3 — quality under `netem`

Two back-to-back 200-call TLS+SRTP runs. Quality is read as **histogram deltas**
around each run, so the 700 calls already in the counters cannot flatter the
result.

```sh
tc qdisc replace dev eth0 root netem delay 20ms 5ms loss 0.5%   # Box B egress
```

| | clean LAN | netem 20 ms ±5 ms, 0.5 % loss |
|---|---|---|
| Jitter mean | 0.44 ms | **3.35 ms** |
| Jitter p50 / p95 / p99 | ≤1 / ≤1 / ≤2 ms | ≤5 / ≤5 / ≤5 ms |
| Jitter distribution | ≤1 ms 96.4 % | ≤5 ms 98.2 % |
| MOS mean | 4.441 | 4.418 |
| MOS below 4.4 | 0 % | 0.6 % |
| CDR `rx_packets_lost` | **0.0** (max 0 over 200 calls) | **73.2** mean (53–93) |
| CDR `rx_packets_received` | 14,780.0 | 14,705.4 |
| Worst call `mos_estimate_min` | 4.436 | 4.339 |

**The result here is not the MOS number — it is that the telemetry is
trustworthy.** The daemon measured **0.495 % loss against 0.5 % injected**
(73.2 of 14,778.6 offered), and reported **exactly zero** loss on all 200 clean
calls. A counter that reports both the impairment you caused and the absence of
one you did not is what §10.2 set out to establish.

Two readings to guard against:

- **MOS moved only −0.023, which is not "impairment had no effect."** G.711 at
  0.5 % loss and 5 ms jitter genuinely is still good audio. The loss and jitter
  counters are where the effect is visible, and they moved sharply.
- **The 20 ms delay correctly did *not* move the jitter histogram.** RFC 3550
  interarrival jitter responds to the ±5 ms variation, not to constant latency,
  which is why ~3.3 ms appears rather than ~20 ms. That is a consistency check
  passing. Latency must be measured as RTT or `sdp_negotiate_seconds`, not here.

**Impairment was one-directional.** It sat on Box B's egress — the
caller→SiphonAI path, which is exactly what `siphon_ai_rtp_rx_jitter_ms`
measures. The return path was clean, so nothing here characterises the daemon's
*transmit* behaviour under loss.

---

## The claim this earns

Filling in §10.4's template for tiers 1 and 2 (tier 3 is not run):

```
200 concurrent calls on 4 vCPU at 0.53 % CPU/call
  (SIPp loopback, G.711, plaintext, no recording).
Validated from a separate FreeSWITCH box at 200 concurrent:
  +23 % CPU per call for the network and a real stack (0.650 %/call),
  TLS + SRTP under 10 % on top and not separable from noise at 200 (0.663 %/call).
Under 20 ms ±5 ms jitter and 0.5 % loss: MOS 4.42, jitter p95 5 ms,
  and loss reported to within 1 % relative of the injected rate.
RTP port range caps concurrency at range/2 — size it for your target.
```

## Not measured

- **No soak at tier 2.** Each step is three 10-second CPU samples inside a
  5-minute hold. This establishes per-call cost and linearity; it says nothing
  about hour-scale behaviour. §6.1/§6.3 remain tier-1-only.
- **A real network.** 0.27 ms between the boxes means this is a real NIC, a real
  kernel path and a real stack — but *not* a WAN. Every impairment figure here
  is synthetic, applied one hop away.
- **Latency.** No RTT or setup-latency measurement was taken; the netem delay is
  invisible to the metric this phase captured.
- **Transmit-side impairment**, per phase 3's one-directional note.
- **Past 200 concurrent.** Also note the generator's own headroom narrowed
  sharply under crypto: FreeSWITCH at 200 TLS calls sits at 102.9 % of one core
  (25.7 % of its box). Comfortable now, but a ~400-call TLS run would start
  making Box B the constraint, and would first need §10.2's
  `max-sessions` / `sessions-per-second` raise — which this run did **not**
  need, since the FS defaults (1000 / 30) already cover 200 at 10 cps.
- **Tier 3** — a live carrier. Unchanged from §10.3: ticket it first.
- **§11's human reference call**, which is the natural next step now that tier 2
  can stage the load.

## Reproducing

The rig is in `tier2/` — `ramp.sh` and `ramp-tls.sh` (Box B), the phase drivers
and the two lab configs (Box A). See `tier2/README.md`; the setup that is easy
to get wrong (FS TLS enablement, the cert, `srtp = "required"`) is written down
there.

## Traps found (also folded into §8)

- **`ps -o %cpu` is a lifetime average, not an instantaneous rate.** It made a
  flat 50-call run look like it was ramping (18 → 24 → 26 %) and is meaningless
  for a FreeSWITCH process that has been up for hours. Use `/proc/<pid>/stat`
  fields 14+15 over a fixed window.
- **`bc` is not installed on either box.** Do the arithmetic in `awk`.
- **`fs_cli -x "show channels count"` prints a leading blank line** — `head -1`
  silently yields nothing. Use `tail -1`.
- **Forcing SIGTERM during the daemon's 30 s drain orphans calls** — no BYE, no
  CDR, and channels stay up on the generator until FS's media timeout. Wait the
  drain out.
- **Always arm a detached auto-removal alongside `netem`.** It impairs your own
  ssh to the box; a lost session must not strand the qdisc.
