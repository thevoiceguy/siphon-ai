# Spike: forge-engine bidirectional audio tap

**Status:** complete (Week 1).
**Risk register entry:** `R1` from `DEV_PLAN.md` §13 — *resolved*.

## Question

Does `forge-engine` expose a clean bidirectional audio tap we can hook into,
or do we need a significant upstream PR?

## Answer

**Yes — and it is exactly the shape we need.** The crate `forge-engine`
exports a public `MediaBridgeManager` / `MediaBridgeHandle` pair that already
sits between forge's RTP forwarding loop and an external consumer. The forge
HTTP-API crate (`forge-api`) ships a working WebSocket route that uses this
manager today; the SiphonAI media path is a near-clone of that route, just
with our protocol shape on the wire.

No upstream PR is required to *unblock* SiphonAI. There is one
nice-to-have PR called out at the bottom of this doc.

## Integration point

```
RTP socket ─► forge-rtp::recv ─► forge-engine::ForwardingEngine
                                       │
                                       ├─► (decode codec, run jitter)
                                       │
                                       │  inbound:
                                       └─► MediaBridgeManager
                                              .try_send_inbound_frame(call_id, InboundMediaFrame)
                                                                │
                                                                ▼
                                                       MediaBridgeHandle
                                                              .recv_frame() ──► SiphonAI bridge crate ──► WS server

   WS server ──► SiphonAI bridge crate ──► MediaBridgeHandle
                                                .send_audio(OutboundMediaFrame)
                                                                │
                                                                ▼
                                  MediaBridgeManager.try_recv_outbound_frame
                                              ▲
                                              │  (ForwardingEngine::drain_media_bridge_outbound)
                                              │
RTP socket ◄─ forge-rtp::send ◄─ forge-engine ─┘
```

Reference: `forge-engine/src/media_bridge.rs` (the manager itself, ~290
lines), `forge-engine/src/forwarding.rs` lines 316–337 (inbound emission)
and 508–531 (outbound drain), and the consumer pattern in
`forge-api/src/routes/media_websocket.rs`.

## Frame types

```rust
pub struct InboundMediaFrame {
    pub leg: ParticipantLabel,        // A or B (see "single-leg" note below)
    pub codec: forge_core::AudioCodec,
    pub payload_type: u8,
    pub sample_rate: u32,             // already-resampled rate
    pub timestamp: u32,               // RTP timestamp
    pub sequence_number: u16,         // RTP seq
    pub samples: Vec<i16>,            // PCM16, mono
}

pub struct OutboundMediaFrame {
    pub target: MediaTarget,          // A | B | Both
    pub sample_rate: u32,
    pub samples: Vec<i16>,
}
```

`forge-engine` emits frames already decoded to PCM16 mono with the codec/PT
metadata preserved, which is exactly what our protocol needs (`docs/PROTOCOL.md`
mandates PCM16-LE mono). Sample rate is per-frame, not call-wide.

## Frame cadence and chunking

Forge does *not* guarantee 20 ms chunks; the inbound frame size depends on
the codec packetization (typically `ptime=20` in SDP). Our bridge crate
must *re-frame* outbound chunks to exactly 20 ms before they hit the WS,
and accept inbound chunks of any size from the WS and re-frame them to
whatever forge expects on the way back in. That re-framing lives in
`crates/media-glue` (or `crates/bridge` — TBD; see Week 2).

## Out-of-band events

`MediaBridgeHandle` carries **only** PCM frames. Other events (DTMF,
session state changes, hangup, codec renegotiation) flow via forge's
`ForgeEvent` broadcast bus (`forge_core::ForgeEvent`). The reference
WS route subscribes to that bus alongside the bridge handle and merges
both streams in a `tokio::select!`. SiphonAI will follow the same shape:
one `select!` arm per (`bridge.recv_frame`, `event_rx.recv`,
`ws_socket.recv`).

Relevant events for v1:
- `ForgeEvent::DtmfDigitDetected` → emit our `dtmf` WS event.
- `ForgeEvent::SessionTerminated`, `SessionActive`, `SessionCreated` →
  drive the call state machine.

## Single-leg vs two-leg

`MediaBridgeManager` is built for forge's two-leg (B2BUA) forwarding model:
inbound frames carry a `leg: A | B` label, and outbound frames pick a
target (`A`, `B`, or `Both`).

SiphonAI is logically single-leg — there's the SIP caller and the WS
server, no second SIP participant. Two viable approaches:

1. **Use a synthetic `B` participant** with a loopback or null RTP socket.
   Forge's forwarding only emits `InboundMediaFrame` for sides that
   actually receive RTP, so a quiet `B` produces no spurious frames.
   Outbound frames target `MediaTarget::A` (the SIP caller).

2. **Add a single-leg session mode upstream**, eliminating the dummy `B`.
   Cleaner but a non-trivial forge PR.

**Decision:** start with (1). The synthetic-B pattern is what the
existing `forge-api` WS route effectively does — it doesn't care whether
B is real, it just plumbs whatever `MediaBridgeHandle` gives it. If we
hit pain we'll consider (2) post-v0.1.0.

## Hot-path concerns

CLAUDE.md §4.3 forbids allocations, locks, and blocking I/O on the audio
path. Audit of the forge surface against that bar:

| Concern | Today | Action |
|---|---|---|
| `Vec<i16>` per frame | Allocates on every frame, both directions | **Accept for v0.1.0.** Pool in a future upstream PR. At 50 fps × 320 samples × i16 ≈ 32 KB/sec/call of allocations — measurable but not pathological. |
| Channel backpressure | `try_send` (non-blocking) on inbound, returns `ResourceLimit`; outbound is `mpsc::Sender::send` (await). | Outbound `send().await` is a yield, not a block — fine. We must add a metric on full-queue drops. |
| Default queue depth | 64 frames ≈ 1.28 s at 50 fps | **Override to 10** via `MediaBridgeManager::with_capacities(10, 10)` so we keep ≤200 ms of buffering as the dev plan calls for. |
| `Mutex<Receiver>` on outbound | Held briefly inside `try_recv_outbound_frame`, lock-free in `send_audio` (sender is cloned). | Acceptable. The receiver-side lock is held only by forge's drain loop. |

## Transitive AI dependency

`forge-engine` directly imports `forge_ai_stream::OpenAIConnector` in its
`ai_integration` module — this is **not** feature-gated. By depending on
`forge-engine`, SiphonAI's Cargo graph transitively pulls in OpenAI,
Anthropic, ElevenLabs, and Deepgram WebSocket clients.

We never *call* any of those — the binary statically excludes them via
dead-code elimination — but it violates CLAUDE.md §4.1's "zero
dependencies on AI vendors" in spirit (Cargo dep-tree, not just runtime).

**Recommendation:** open a small upstream PR adding an `ai` Cargo feature
on `forge-engine` that gates the `ai_integration` module and the
`forge-ai-stream` dep. SiphonAI then consumes
`forge-engine = { ..., default-features = false, features = ["g722"] }`.

This is a Week-1 follow-up but does not block the spike or any v0.1.0
work.

## Verification

The proof point is `crates/media-glue/tests/loopback.rs` — a smoke test
that drives synthetic frames through `MediaBridgeManager` end to end:
attach, push inbound frame from the "RTP side", receive on the
application side, queue outbound, drain back out, and assert the round
trip.

Run with `cargo test -p siphon-ai-media-glue`.

## Net result

Week-1 risk R1 ("forge-engine doesn't expose a clean tap") is resolved.
The remaining Week-1 work — wiring siphon-rs UAS to a forge `MediaSession`
with the bridge attached, and bouncing audio in a SIPp scenario — has no
new unknowns. Dev plan stands.
