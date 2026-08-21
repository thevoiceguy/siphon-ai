# §10.3 tier 3 — a live Twilio trunk, 0.49.9

Run 2026-08-21 against **prod's shipped 0.49.9** (`/usr/bin/siphon-ai`
sha256 `ca7a6af19…`) on the reference node, over the live Twilio Elastic SIP
Trunk.

Scope: `LOAD_TEST_PLAN.md` **§10.3 / §10.3.1** — the last unrun section of
this plan. What a carrier uniquely adds is a real SBC and the public
internet; both are per-call constants, which is why a small run suffices.

**Result: pass, 60/60.** Every call connected, tore down cleanly, and the
carrier path proved *milder* than the netem model tier 2 used to
approximate it.

> **This run dialled the public PSTN and billed both legs of every call.**
> Twilio support was notified in advance with the exact traffic profile and
> confirmed it needs no approval or account flag while it stays inside the
> account's limits.

---

## The shape, and why it is not the shape §10.3 describes

§10.3 asks for 15 minutes at 30–50 concurrent. **This account cannot do
that**, and the reason is worth recording: Twilio's concurrent-channel cap
follows the Customer Profile — an approved *Business* profile is effectively
unlimited, but *Individual* is **3**, unapproved 2, trial 4. This account is
Individual.

A trombone (originate to our own DID so the call returns as a second leg)
occupies **two** channels, so this ran **one call at a time**, sequentially,
with one channel deliberately left free so a genuine inbound call would not
be rejected mid-test.

That trade is better than it sounds for the thing tier 3 measures. Sixty
sequential calls give **60 latency samples spread over 35 minutes**, where
the concurrent form would give 60 set up inside one 60-second window. For
per-call constants, spread beats simultaneity.

What it costs is any statement about carrier concurrency — see the claim at
the bottom, which does not make one.

| | |
|---|---|
| Node | `siphon-ai 0.49.9`, 4 vCPU / 7947 MB, Debian 13 |
| Trunk | Twilio Elastic SIP Trunking, TLS + SRTP (`srtp = "required"`), IP-ACL authorised |
| Path | originate → Twilio SBC → our own DID → back in to the same node |
| Calls | 60 sequential, 30 s hold, 1 at a time (2 of 3 channels) |
| Sinks | `driver_bot.js ?mode=sustain` on both legs — 50 fps filler, no AI spend |
| Posture | prod's own: HEP, webhooks, quality records, neural VAD on the inbound route |
| Cost | ~60 call-minutes across both legs |

---

## Carrier setup latency

The number tier 3 exists to produce. From the outbound CDR,
`answered_at − started_at`:

| | min | p50 | **p95** | max |
|---|---|---|---|---|
| setup (INVITE → 200 OK) | 324 ms | 384 ms | **460 ms** | 524 ms |

n=60, so the p95 is the 57th sample of 60 — a real percentile, but read it
as "the slow end of sixty calls", not as a smooth tail.

**The webhooks decompose it further**, which the CDR alone cannot. Our
`outbound_initiated` to the returning leg's `call_start` is the carrier's
own out-and-back, with our answer excluded:

| | min | p50 | **p95** | max |
|---|---|---|---|---|
| carrier round trip | 260 ms | 281 ms | **383 ms** | 403 ms |

So of a typical 384 ms setup, **~281 ms is Twilio** and the remaining
~100 ms is our own answer propagating back out. That split matters: it says
the carrier, not the daemon, owns three quarters of outbound setup time, and
no amount of local optimisation will move it.

## Quality — and the netem model was pessimistic

Per-call `quality` blocks, both legs of all 60 calls:

| | outbound leg | returning leg |
|---|---|---|
| MOS p50 | **4.435** | **4.435** |
| MOS range | 4.434 – 4.435 | 4.434 – 4.436 |
| jitter p50 | **1.94 ms** | **1.94 ms** |
| jitter p95 | 2.17 ms | 2.15 ms |
| jitter max | 2.25 ms | 2.19 ms |
| packet loss | 1 / 88,104 = **0.001 %** | 1 / 89,398 = **0.001 %** |
| calls with any loss | 1 of 60 | 1 of 60 |

Against the two reference points tier 2 established:

| | clean LAN (tier 2) | **live carrier (here)** | netem 20 ms ±5 ms / 0.5 % (tier 2) |
|---|---|---|---|
| jitter | 0.44 ms | **1.94 ms** | 3.35 ms |
| MOS | 4.441 | **4.435** | 4.418 |
| loss | 0.0 % | **0.001 %** | 0.495 % |

🔑 **The real carrier path sits between the clean LAN and the impairment
model, and much closer to the LAN.** Jitter is 4.4× the LAN figure but only
58 % of the netem figure, and loss is **500× lower** than the 0.5 % netem
injects.

🔑 **So tier 2's netem numbers are a conservative bound, not a prediction.**
That is the right way to read them and it was not previously demonstrated —
publishing an impairment-model result alongside a live one is the only way
to know which side of reality the model sits on. On this path, it is the
pessimistic side.

⚠️ **One path, one carrier, one afternoon.** A different region, a congested
hour, or a different carrier's SBC could land anywhere. This does not
generalise; it calibrates.

## Daemon behaviour

- **60/60 answered**, zero originates refused, zero trombones that failed to
  close.
- Termination: 60 `local_shutdown` (outbound legs, we hung up) + 60
  `caller_hangup` (returning legs, Twilio hung up when we did).
- **Zero WARN or ERROR attributable to the run.** The four in the window are
  internet scanners hitting the public IP and being correctly 403'd for no
  matching trunk, plus one rustls SNI complaint from the same scan.
- `dialogs_active` returned to **0** — the #548 fix (`RESULTS-0.49.9-outbound.md`)
  holding across 120 legs on a live carrier, not just loopback.

**No per-call CPU figure**, deliberately. §10.3 asks for one against the
tier-2 TLS+SRTP number, and it cannot be honestly produced here: at one
concurrent call the fixed overhead dominates and the posture differs from
tier 2's lean one (HEP, webhooks, quality records and neural VAD are all on).
A CPU/call number from this run would be a comparison of postures wearing
the clothes of a comparison of transports.

---

## The claim this earns

Per §10.4, adjusted to what was actually measured:

> 200 concurrent calls on 4 vCPU (SIPp loopback, G.711, plaintext, no
> recording). Validated from a separate FreeSWITCH box over TLS + SRTP at
> 200 concurrent: 0.663 % CPU per call, MOS p50 4.418 under 20 ms / 0.5 %
> netem impairment. **Confirmed against a live Twilio trunk, 60 sequential
> calls: setup p50 384 ms / p95 460 ms, of which ~281 ms is the carrier;
> MOS p50 4.435, packet loss 0.001 %.** RTP port range caps concurrency at
> range/2 — size it for your target.

Note what it does **not** say: nothing about carrier concurrency, because
none was measured. Tier 2 carries the concurrency scaling.

## Traps

🪤 **`sdp_negotiate_seconds` is not carrier setup latency**, though §10.3
named it until this run. It times `prepare_call` — negotiate, port alloc,
tap attach — which is our own local work. Use the CDR's
`answered_at − started_at`, and the `outbound_initiated` → `call_start` gap
to isolate the carrier.

🪤 **Filter *both* the CDR and the webhook file by their start line.**
`tier3_stats.py` filtered only the CDRs on its first outing and quietly
mixed the earlier smoke call into the round-trip figures — visible only
because the sample count came out larger than the call count.

🪤 **A trombone costs two channels.** On a 3-channel trunk that is one call
at a time. Sizing a run by the channel cap rather than the *call* cap will
have the carrier reject half of it.

🪤 **Point the outbound leg at a WS sink that generates audio.** With echo
sinks on both ends of a trombone, nothing ever originates sound and the
loop carries silence — measurable RTP, meaningless MOS. `?mode=sustain`
fills at 50 fps.
