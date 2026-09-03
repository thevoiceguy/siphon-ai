# HEP / Homer Integration

[Homer](https://github.com/sipcapture/homer) is the SIP-world packet-flow
explorer; HEP3 (Homer Encapsulation Protocol v3) is the wire format Homer's
collector — `heplify-server` — speaks. SiphonAI ships HEP3 packets at it
so operators can see a call's SIP signaling, RTCP, and SiphonAI-side
application events on one correlated timeline, keyed by SIP `Call-ID`.

## Architecture

Three layers emit HEP3 packets at the configured collector, all sharing
one queue and one UDP socket through the daemon-wide `HepSink`:

```text
   ┌──────────────┐
   │ siphon-rs    │ ──► SIP messages (parsed inbound + serialized outbound)
   │   sip-hep    │     HepProtocol::Sip (chunk type 0x01)
   └──────────────┘
   ┌──────────────┐
   │ forge-media  │ ──► RTCP packets observed on the call (SR/RR/SDES/BYE)
   │  forge-hep   │     HepProtocol::Rtcp (chunk type 0x05)
   │              │ ──► Per-RR RTP-QoS summary (ssrc, jitter, loss)
   │              │     HepProtocol::RtpQos (vendor chunk type 0x20)
   └──────────────┘
   ┌──────────────┐
   │ siphon-ai    │ ──► Per-call lifecycle log lines
   │ (telemetry)  │     HepProtocol::Log (chunk type 0x64)
   │              │ ──► Full CDR JSON when a call ends
   │              │     HepProtocol::Cdr (chunk type 0x65)
   │              │ ──► STIR/SHAKEN verdict JSON per inbound call
   │              │     HepProtocol::Verstat (chunk type 0x66)
   └──────────────┘
                  └────────────► HepSink ──UDP──► heplify-server
                                       (Arc-shared, single worker task)
```

The `hep-rs` crate owns the codec, the `HepSink` trait, and the UDP/TCP/TLS
transports. The three emitters above are thin: they construct a `HepPacket`
with the right protocol byte and forward it to the sink. All three carry
the same SIP `Call-ID` as the correlation key, which is what Homer's UI
threads together into one call view.

## Best-effort, always

HEP emission must never block the audio path (CLAUDE.md §4.7). Concretely:

- Sink methods are non-blocking. A full queue drops the packet and ticks
  `siphon_ai_hep_packets_dropped_total{reason="queue_full"}`.
- The daemon does NOT log every drop or every failed send. One warning per
  minute max, from the `hep_rs::udp` target. (CLAUDE.md §4.7.)
- The audio path never calls into the sink synchronously — forge's RTCP
  recv loop queues and returns; a worker task handles the actual UDP send.

### What the metrics can and cannot tell you

Two series are exported, both mirrored every 10 s from the sink's own
counters (they live upstream because SIP chunks come from `sip-hep` and
RTCP/QoS from `forge-hep`, neither of which passes through siphon-ai):

| Metric | Means |
|---|---|
| `siphon_ai_hep_packets_sent_total` | packets written to the wire |
| `siphon_ai_hep_packets_dropped_total{reason="queue_full"}` | producer dropped on a full `[hep].queue_capacity` |
| `siphon_ai_hep_packets_dropped_total{reason="collector_down"}` | the socket refused the send — `ECONNREFUSED` from a dead collector's ICMP port-unreachable (0.51.0, #596) |
| `siphon_ai_hep_collector_up` | `1` unless a send was refused during the last 10 s sample interval, then `0` (0.51.0) |

All of them exist on `/metrics` from startup, before any call — so an
alert can be written against them directly rather than having to tolerate
an absent series. Every packet handed to the sink lands in exactly one of
`sent`, `queue_full`, or `collector_down`.

**Alert on `siphon_ai_hep_collector_up == 0`.** Before 0.51.0 a refused
send was counted nowhere and the only evidence was a throttled log line —
which any target-named log filter muted (#460, and again in #596). The
line is still there:

```
WARN hep_rs::udp: HEP UDP send failed; further failures suppressed until
     throttle elapses collector=127.0.0.1:9060 error=Connection refused
```

and the daemon now keeps a `warn` floor under every log filter so it
cannot be muted by accident (#597) — but the gauge is the signal to page
on.

**Why the gauge is derived over a window, not per send.** The sink writes
to a *connected* UDP socket, so a dead collector's refusal comes back as
`ECONNREFUSED` on the *next* send — one send consumes the queued ICMP
error, the following one finds the queue empty and succeeds. Measured
against a dead collector on the same host, `sent` kept climbing at almost
exactly **half** rate (35 of ~70 attempted) rather than stopping, and
`collector_down` takes the other half. A point sample of "did the last
send succeed" would therefore flap; the gauge instead asks whether *any*
send failed since the previous sample. Don't alert on `sent` going flat —
a halving is not something a threshold can distinguish from a quiet
period.

**What UDP still cannot tell you.** `sent` counts wire-level success, so a
collector that is up but discarding — a heplify-server that isn't writing
to storage — still counts as sent, and a collector that is *black-holed*
(a firewall dropping silently, no ICMP back) produces no refusals at all:
`collector_up` stays 1. For those two, steps 5–6 below are the check.

## Configuration

```toml
[hep]
enabled          = true
collector        = "homer.example.com:9060"
capture_id       = 2001              # the agent_id heplify-server sees
capture_password = "${HEP_PASSWORD}" # bytes 0x0011 (HEP_AUTHKEY chunk)
queue_capacity   = 256               # default; bump for bursty environments
```

Validation happens at config load (`siphon-ai-config::compile_hep`):
`collector` must be a parseable `host:port`, the host must resolve, and
`capture_id` is required when `enabled = true`. A misconfigured `[hep]`
block keeps the daemon from starting — better that than silent drops.

## `capture_id` conventions

`capture_id` is HEP3's tenant key — Homer uses it to scope a captured
packet to one "agent" in its UI. Two viable patterns:

- **One per node.** Most deployments. Each SiphonAI box gets a unique
  integer; the SIP `Call-ID` correlates across multiple agents inside a
  single call view (if it ever traverses two SiphonAI nodes).
- **One per route.** A multi-tenant setup where Homer needs to scope by
  customer. Requires patching the daemon to vary `capture_id` per call;
  not in v1. Open an issue if you need it.

The dev plan §15 #6 calls this out: revisit when a multi-tenant Homer
user surfaces.

## What appears in Homer's UI

For a `basic_call_then_bye` SIPp scenario (INVITE / 100 / 200 / ACK / BYE /
200, no audio), Homer renders:

- A ladder diagram of all six SIP messages, keyed by `Call-ID`.
- A timeline of the SiphonAI Log chunks (`call_started`, `call_ended`,
  termination cause).
- The CDR JSON as an inspectable record at call end.

When the scenario also exchanges RTP (`uac_with_rtp.xml`, post-v1), the
QoS panel populates from forge's per-RR `RtpQos` chunks: jitter,
fractional loss, cumulative loss, the SSRC of each direction.

## STIR/SHAKEN verstat chunk (0x66)

When `[security.stir_shaken].enabled = true`, the accept path ships one
`HepProtocol::Verstat` (chunk type 0x66) per inbound call, correlated by
SIP `Call-ID`, so the verdict threads onto the same call view as the SIP
ladder and the CDR. The payload is the verdict as JSON — the same shape as
`start.verstat` (PROTOCOL.md) and the `verstat_*` CDR fields:

```json
{ "attest": "A", "orig_tn": "+12155551212", "orig_passed": true,
  "dest_passed": true, "cert_chain_valid": true, "signature_valid": true }
```

The chunk is emitted for **every** inbound call while verification is on —
including unsigned calls (`signature_valid: false`, `attest` absent) and
gate-rejected ones (the verdict is exactly what an investigator wants when
a call was screened). `attest` is the *claimed* level; trust it only when
the booleans all hold. No chunk is emitted when verification is disabled.

## Local validation

`examples/homer-stack/` is a full Homer + heplify-server + Postgres compose
stack. Spin it up alongside the daemon and place a call:

```sh
cd examples/homer-stack
docker compose up                    # Homer at http://localhost:9080

# In another shell, point the daemon at the local heplify
cargo run -p siphon-ai -- --config examples/homer-stack/siphon-ai-hep.toml

# Place a call (any softphone or SIPp scenario)
sipp -sf test-harness/sipp-scenarios/basic_call_then_bye.xml \
    -m 1 -p 5080 -s 1000 127.0.0.1:5060
```

The call appears in the Homer UI within a few seconds. If it doesn't,
work the diagnostics in order:

1. The startup line — `HEP3 shipping active (...) collector=... capture_id=...`.
   No line means `[hep].enabled` is false or the config never loaded, and
   nothing below will tell you anything.
2. `siphon_ai_hep_collector_up` from `/metrics` — `0` means the daemon is
   emitting and the collector is refusing (not listening on that
   address/port). Check the address, the firewall, and whether
   heplify-server is bound to 9060/udp. `journalctl -u siphon-ai | grep
   hep_rs` shows the matching `HEP UDP send failed ... Connection refused`
   warning with the error text.
3. `siphon_ai_hep_packets_sent_total` from `/metrics` — should climb on
   every call. Dead flat while calls are being handled, with no warning
   from step 2, means nothing is being *emitted* (wrong `[hep]` config)
   rather than nothing being delivered. Note this only distinguishes
   "emitting nothing" from "emitting" — a collector that is down still
   moves this counter (see above), so it cannot stand in for step 2.
4. `siphon_ai_hep_packets_dropped_total{reason="queue_full"}` — climbing
   means `[hep].queue_capacity` (default 256) is too small for the call
   rate. Packets are being lost before the wire; raise it.
5. `heplify_*` metrics from heplify-server's own `/metrics` — confirms
   packets arrived. If step 3 climbs and this doesn't, the loss is on the
   network path, not in siphon-ai.
6. Postgres `hep_proto_1_default` table — `SELECT count(*)` shows whether
   SIP chunks landed in storage.

Step 2 answers "refusing"; steps 5–6 answer "black-holed or discarding",
which UDP cannot report from the sender's side — see the section above.

## Adding new emissions

The chunk types above cover v1. If you have an event Homer should
know about that doesn't fit them — say, a `route_match` event with the
matched route name — the right path is:

1. Confirm whether HEP3 defines a chunk type for it (vendor-specific
   chunks live in the 0x10–0xff range; the IANA-assigned chunks are
   listed in [RFC HEP3 draft](https://github.com/sipcapture/homer/wiki/HEP-Specifications)).
2. Add the encoding in `hep-rs` if it isn't there yet (PR upstream).
3. Wire the emission from the right layer:
   - SIP message → siphon-rs (`sip-hep`).
   - RTCP / QoS → forge-media (`forge-hep`).
   - Application event / log / CDR pointer → SiphonAI's
     `crates/telemetry/src/hep.rs`.
4. Verify the chunk shows up in Homer correlated with the call's
   `Call-ID`.

CLAUDE.md §7.8 has the full checklist.
