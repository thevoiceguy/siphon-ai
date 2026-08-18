# §11 reference call — a live PSTN canary at 200 concurrent

Run 2026-08-18 against the **shipped 0.49.0 `.deb`** (sha256 `caf0f6bb1…`,
byte-identical to the release tarball binary), on the reference node, driven by
the tier-2 FreeSWITCH generator.

Scope: `LOAD_TEST_PLAN.md` **§11** — stage the load, then place one more call
from a real handset over the live Twilio trunk and use it normally while the box
is saturated. Run as the **§11.2 A/B**: the identical call with the load off,
then again at 200 concurrent.

**Result: pass.** `first_audio_out_ms` was **identical** idle vs loaded (21 ms
both), MOS moved **+0.0%**, zero packets were lost in either direction, and the
caller's verdict was *"echo sounded the same as idle."*

This is a canary, not a benchmark. The 200 are the load; call 201 is the probe.
For the knee, the soak and the leak audit see `RESULTS-0.48.18.md`,
`RESULTS-convergence-8h.md` and `RESULTS-tier2.md`.

---

## Environment

| | Box A — under test | Box B — generator |
|---|---|---|
| Hardware | 4 vCPU, 7947 MB | 4 vCPU, 7.9 GB |
| OS | Debian 13 (trixie), Linux 6.12.95+deb13-amd64 | Debian 12 |
| Software | `siphon-ai 0.49.0` (shipped deb) on `:5060` udp / `:5061` tls | FreeSWITCH 1.11.0 `mod_sofia`, external profile `:5080` |
| `ulimit -n` | 524288 | |

Config knobs that materially change the result:

| Knob | Value |
|---|---|
| `rtp_port_range` | `[40000, 41000]` — 500 pairs, widened from 250 for this run |
| `[media].codecs` | `["pcmu", "pcma"]` (G.711) |
| `[media].srtp` | `preferred` (inbound); `required` on the Twilio gateway |
| `inactivity_timeout_secs` | 60 |
| `[recording].mode` | **`off`** globally, `always` on the reference-call route only |
| `[hep].enabled` | **true** |
| `[route.media].vad` | **`neural`** on the reference-call route |

### ⚠️ Posture is NOT the §2 baseline — the CPU number is not comparable

§2's baseline posture for §§3–5 is recording off, HEP off, energy VAD, no
webhooks, no audit. This run is deliberately the opposite: it is a
**production-shaped node**, with HEP, webhooks, audit, quality records and
neural VAD all live, because §11's whole point is to probe the box a real
caller would reach.

So the **136.9% of one core measured here at 200 concurrent must not be compared
to tier-2's 129.9%** at the same concurrency — that run used the lean posture.
The delta is posture, not a regression, and this run is not evidence either way.

---

## Topology — why two WS servers

```
   200 load calls   Box B FreeSWITCH ──LAN, plaintext──► :5060 ──► paced_sink.mjs :8767
   the canary       handset ──PSTN──► Twilio SBC ──TLS+SRTP──► :5060 ──► driver_bot.js :8081
```

§11.4 requires the canary's WS server not be the saturated one carrying the
load, and §1.4 requires the sink not be the bottleneck. Two separate processes
satisfy both. The load route was overridden to `:8767` specifically to keep the
200 **off** the node's real LLM bot on `:8080` — pointing them there would have
billed live AI spend and measured that bot's scheduling rather than the bridge's.

`srtp_profile` on the canary read `AES_CM_128_HMAC_SHA1_80` on both A and B
calls, confirming §11.1's claim that this test exercises a path the LAN load
never touches: real carrier SBC, TLS signalling, SRTP media.

---

## Methodology finding: barge-in makes echo mode unusable as a canary

**The first idle call failed as a test and succeeded as a bug hunt.** The caller
reported *"the echo was chopped off, i heard it but not the complete audio."*

The node runs `[bridge.barge_in] mode = "pause"`, which suppresses outbound
playout whenever the caller speaks. In echo mode the outbound audio **is** the
caller's own voice, so every spoken word cut its own echo. The evidence was
unambiguous:

| | caller (rx) | bot (tx) | retention |
|---|---|---|---|
| barge-in `pause` | 27 utterances / 6.4 s | 15 utterances / 2.8 s | **44%** |
| barge-in `notify_only` | 26 utterances / 5.3 s | 30 utterances / 4.7 s | **89%** |

`barge_in_count` was **20** on the chopped call — exactly one per number counted
— and `tx_packets_sent` was 1770 against the ~2430 that 48.6 s of continuous
50 fps would produce, i.e. ~13 s of playout deliberately suppressed, ~660 ms per
utterance (a spoken number plus the 200 ms debounce). The inbound half was
pristine throughout: 2246 packets, **0 lost**.

**Why this had to be fixed rather than tolerated.** The artifact would have
appeared in *both* the idle and loaded calls, so the A/B would still have
compared — but it makes a load-induced dropout indistinguishable from barge-in
cutting the caller's own voice, which is precisely the signal the canary exists
to detect.

The fix is a **per-route** `[route.bridge.barge_in] mode = "notify_only"` on the
reference-call route only: VAD events still flow to the WS server, but barge-in
never touches playout. Production barge-in behaviour is untouched. It is
route-level, so `systemctl reload` applied it with no restart and no dropped
calls (`config_reloads_total{applied} 1`, uptime unbroken).

**Fixed in this PR:** §11.2 now carries this as a step — an echo-mode canary
requires barge-in off on that route, with the `notify_only` override and the
measured retention figures, or the spoken-script test measures the barge-in
policy instead of the bridge.

---

## The A/B — §11.2

Both calls: same handset, same trunk, same spoken script (count *one … twenty*
at ~1/second), same route, same WS server, same barge-in policy.

| Compare | Idle | Loaded (200) | Delta |
|---|---|---|---|
| CDR `quality.first_audio_out_ms` | 21 | **21** | **0.0%** |
| CDR `quality.avg_jitter_ms` | 1.964 | 2.056 | +4.6% |
| CDR `quality.max_jitter_ms` | 2.375 | 2.500 | +5.3% |
| CDR `quality.avg_packet_loss_ratio` | 0.0 | **0.0** | — |
| CDR `quality.max_packet_loss_ratio` | 0.0 | **0.0** | — |
| CDR `quality.avg_rtcp_rtt_ms` | 15.843 | 14.164 | **−10.6%** |
| CDR `quality.mos_estimate_min` | 4.43467 | 4.43498 | +0.0% |
| CDR `quality.mos_estimate_avg` | 4.43562 | 4.43580 | +0.0% |
| CDR `quality.rx_packets_lost` | **0** / 1747 | **0** / 2249 | — |
| Recording: rx utterances | 26 | **26** | — |
| Recording: echo retention (tx/rx) | 88.7% | **90.2%** | +1.5 pt |
| Subjective 1–5 | reference | *"sounded the same as idle"* | — |

Call durations differed (35.5 s vs 46.9 s), which is why the raw packet counts
rise 28.7%; that is call length, not a quality signal. **Compare ratios, not
absolute packet or active-audio totals, across calls of unequal length** — an
absolute "tx seconds" comparison reads as a spurious +17% improvement.

`first_audio_out_ms` is the row §11.2 says to watch hardest, and it did not move
at all. `avg_rtcp_rtt_ms` *improving* by 10.6% under load is the tell that the
jitter deltas here are carrier-path variance rather than scheduler pressure —
the absolute changes are 0.09 ms and 0.13 ms.

---

## §4 SLOs at 200 concurrent

| SLO | Threshold | Measured | |
|---|---|---|---|
| Setup success | 100% reach `200 OK` | **207/207 accepted**, 0 rejected | ✅ |
| Media quality | MOS p50 ≥ 4.0 | all **8090** samples > 4.4; mean **4.441** | ✅ |
| Loss (from CDR) | p95 ≤ 0.01 | **0.0** on the canary, 0 packets lost | ✅ |
| Jitter | `rtp_rx_jitter_ms` p95 ≤ 30 ms | **p95 ≤ 1 ms** (8184/8516 ≤ 1 ms), mean **0.284 ms** | ✅ |
| Setup latency | `sdp_negotiate_seconds` p95 ≤ 200 ms | **p95 ≤ 1 ms** (204/207 ≤ 1 ms), mean 0.99 ms | ✅ |
| Headroom | CPU ≤ 80% of total cores | **34.2%** (136.9% of one core, 4 vCPU) | ✅ |
| Not port-capped | conc < `rtp_port_range`/2 | 200 < 500 | ✅ |
| Playout | see finding below | 69,155 frames dropped — **generator artifact** | ⚠️ |

RSS moved 87.7 MB → 103.4 MB across the loaded run. That is within the
established per-call cost and not read as a leak here; `RESULTS-convergence-8h.md`
is the authority on convergence and a single 10-minute run cannot speak to it.

---

## Findings

### 1. §4's Playout SLO row is stale — the metric exists

The plan stated `siphon_ai_outbound_audio_frames_dropped_total` was
"**no such metric; this SLO has no data source**". It exists — it landed in
0.48.14 via #474 — and read **69,155** during this run. **Fixed in this PR:**
§4's row is now an assertable threshold, with a note on attributing a
non-zero value before reporting it.

### 2. The 69,155 drops are `paced_sink`, not the bridge

Exactly **200** `WARN`s appeared, one per load call:

> `server streaming outbound audio faster than realtime; dropping oldest beyond the 200 ms buffer (PROTOCOL §5.5)`

One-per-call is systematic, not degradation. `paced_sink.mjs` sends to every
connection inside a single 20 ms tick; at 200 connections that loop overruns the
tick, and the monotonic-origin correction then fires immediately and catches up
in a **burst** — which the daemon correctly sees as faster-than-realtime and
trims to its 200 ms buffer.

This is the mirror image of the known `ws_sink.mjs` trap (§8): that sink
*under*-runs at low connection counts; `paced_sink` *over*-runs at high ones.
**§1.4's "prove the sink is not the constraint" is NOT satisfied at 200 by
`paced_sink` in its current form** — it needs per-connection pacing, or sharding
across processes, before a run at this concurrency can call its outbound path
clean. The canary was insulated (separate process on `:8081`) so the A/B stands,
but any *aggregate* outbound-path claim from this run would not. **Fixed in this
PR:** §8 now documents the over-run as the mirror of the existing `setInterval`
under-run trap.

### 3. §11.2's CDR field list is correct

An earlier reading of this run wrongly concluded that `avg_jitter_ms`,
`max_jitter_ms` and `avg_packet_loss_ratio` were absent from CDR v8. They are
present — they are simply **skipped when the leg carries no RTCP**, which is why
the FreeSWITCH LAN smoke calls lacked them and the Twilio leg has them. No plan
change needed; worth a note so the next reader doesn't repeat the mistake.

### 4. Unrelated: REGISTER client transactions are never reclaimed

Not part of §11, found while checking the box before the run. On the
pre-upgrade process, uptime 3 days:

> `Client transaction limit reached, evicting oldest transaction key=… method: Register … current_count=10000 limit=10000`

REGISTER refreshes run 1/min (`expires_secs = 120`), each producing a 401 plus an
authenticated retry. A REGISTER client transaction should terminate in ~32 s, so
steady state ought to be 1–2 live entries, not 10,000; 5,067 successful
registrations over 3 days matches the fill rate. Bounded by the eviction, so
nothing breaks, but the table fills with dead entries and then evicts on every
refresh — and the eviction picks its victim by `start_time` alone, so once the
table is full a live transaction can be the one taken.

Root-caused and filed upstream as **siphon-rs#103**, fixed in **siphon-rs#104**:
the terminal wait timers (K for non-INVITE, D for INVITE) set the FSM
`Terminated` but emit `Cancel`, while the manager removed the entry only on
`Terminate` — so a client transaction that *failed* was reclaimed and one that
*succeeded* was not. Wider than REGISTER (every normally-completing client
transaction, both transports) and client-side only; the server's Timer J/I
already emit `Terminate`. Picked up here on the next siphon-rs bump.

---

## Reproducing

```sh
# generator (Box B) — self-terminating, so a lost session cannot strand calls
/root/ramp-prod.sh 200 10 600      # 200 calls, 10 cps, 600 s hold

# canary: arm echo for the next WS connection, then place the call
curl -sX POST -H 'Content-Type: application/json' \
     -d '{"mode":"echo"}' http://127.0.0.1:8082/nextmode
```

The reference-call route is keyed on the handset's `from_user`, with global
recording off, so **only** the canary records — recording all 200 would turn the
run into the disk-I/O benchmark §11.4 warns about. Verified: the CDR reports
`route=reference-call` and exactly one `.wav` per canary call.

---

## The headline this earns

> A live PSTN call, placed from a mobile handset over a real carrier trunk with
> TLS and SRTP while **200 concurrent calls** were active on the same 4-vCPU
> node, was indistinguishable from the same call placed on an idle box —
> **identical 21 ms first-audio latency, MOS unchanged at 4.435, zero packet
> loss**, and no audible difference to the person holding the phone.
