# SiphonAI 0.2.0 Development Plan

**Status:** working draft. 1 / 6 decisions in §9 resolved (call-progress / early-media — see §9.1); the remaining five gate Sprint 1.

This is the incremental plan from `v0.1.0` (see `docs/DEV_PLAN.md` for the original product definition and locked decisions). It picks up the 0.2.0 roadmap brief and re-organises it around what actually fits siphon-ai's "thin orchestration bridge" mission per `CLAUDE.md` §4.1.

## 1. Cardinal Rule, Restated

`CLAUDE.md` §4.1 still binds: **siphon-ai is a provider-neutral SIP-to-WebSocket bridge. AI providers (STT / LLM / TTS) live in the developer's WS server, not here.**

Three roadmap items need re-casting before they can be planned:

| Roadmap item (as proposed)                                                                                                  | Where it actually belongs                                                                                                                                                                                                                                                                  |
| --------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| **Provider abstraction layer** (STT/LLM/TTS provider switching, configured in siphon-ai TOML)                               | **WS-server reference example.** Ship `examples/voice-agent-toolkit/` (or similar) demonstrating the pattern in Python and Node. siphon-ai itself adds nothing.                                                                                                                            |
| **Higher-level AI events** (`intent_detected`, `caller_frustration`, `voicemail_detected`, `compliance_risk_detected`, etc.) | Split: media-derivable primitives (`silence_detected`, `dead_air_detected`, RTP stats) → siphon-ai event channel. Semantic AI events → emitted *by* the WS server, optionally forwarded via siphon-ai's lifecycle-webhook surface as a generic `event_forwarded` type. siphon-ai does not infer them. |
| **Media-aware orchestration engine**                                                                                        | Not a feature — a north star. Delivered piecemeal across 0.2.0+.                                                                                                                                                                                                                            |

Without this re-casting, the binary gains a `forge-ai-stream`-like dep chain — exactly the thing we stripped out for 0.1.0 (`docs/SPIKE_MEDIA_TAP.md` §"Transitive AI dependency").

## 2. Already Shipped in v0.1.0 (Do Not Re-Build)

A few items the roadmap lists as 0.2.0 must-haves are already in 0.1.0:

- **Hold / resume** — re-INVITE direction-change drives `OutgoingEvent::Hold` / `Resume` (`crates/core/tests/hold_resume.rs`).
- **Blind transfer** — `BridgeIn::Transfer` → `run_transfer` → UAC INVITE to Refer-To target (`test-harness/sipp-scenarios/blind_transfer.xml`).
- **Hangup**, **mute-as-flush via `Clear`**, **DTMF send + receive**.
- **Barge-in / `speech_started`** via forge-vad.
- **WSS (secure WebSocket)** — `tokio-tungstenite` with `rustls-tls-webpki-roots`.
- **SIP over TLS** — `sip-transport` built with `tls`; `[sip].tls` config block.

Treat 0.2.0 as *expanding around* these, not re-implementing them.

## 3. Item-by-Item Disposition

| #   | Roadmap item                                            | Fit                                          | Disposition                                                                                                                                                                                                                  | Target              |
| --- | ------------------------------------------------------- | -------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------- |
| 1   | Configurable call progress (`180`/`183`/early media)    | ✅ Core SIP, perfect fit                      | Add `[sip.call_progress]` config; sip-glue sends the chosen provisional response; early-media path requires forge to RTP-send before 200 OK (see §9).                                                                       | **0.2.0**           |
| 2   | Call handling API (expanded)                            | ✅ Mostly fits, mixed effort                  | Split by maturity — see §3.2.                                                                                                                                                                                                | **0.2.0 / 0.3.0**   |
| 3.1 | Twilio integration example                              | ✅ Docs + example                             | New `docs/INTEGRATIONS_TWILIO.md` + `examples/twilio-trunk-config/`; no daemon changes.                                                                                                                                       | **0.2.0**           |
| 3.2 | Real-time transcription example                         | ✅ Reference WS server                        | New `examples/transcription-server-py/` using Deepgram (or AssemblyAI); demonstrates the non-agent use case.                                                                                                                  | **0.2.0**           |
| 3.3 | AI analytics example                                    | ✅ Reference WS server                        | Similar to 3.2 with sentiment / keyword overlay.                                                                                                                                                                              | **0.3.0**           |
| 3.4 | Live translation example                                | ✅ Reference WS server                        | Bigger — STT + translation + TTS reference pipeline; depends on real-time injection latency feeling acceptable.                                                                                                              | **0.3.0**           |
| 3.5 | Supervisor / whisper agent                              | ⚠️ Needs new bridge primitives              | Whisper requires per-leg audio channels (forge support); example depends on the protocol/forge additions.                                                                                                                    | **0.3.0**           |
| 4   | SRTP + TLS everywhere                                   | Mixed                                        | TLS / WSS already there → **docs + production recipe ✅ 0.2.0**. mTLS, SRTP, cert rotation, STIR/SHAKEN → **0.3.0 / 0.4.0** (forge-media + siphon-rs upstream work).                                                          | Split               |
| 5   | AI provider abstraction                                 | ❌ §4.1 violation in siphon-ai               | Re-cast as a reference WS server (see §1). Ships in `examples/`.                                                                                                                                                              | **0.2.0** (example) |
| 6   | Higher-level AI events                                  | ❌ Pure AI events; ⚠️ media primitives OK   | Media primitives (`silence_detected`, `dead_air_detected`, `rtp_stats`) → **0.2.0**. Semantic events forwarded via webhook → **0.3.0**.                                                                                       | Split               |
| 7   | RTP stats / media quality                               | ✅ Pure media observability                   | forge-engine already collects most; expose as `BridgeOut::RtpStats` event + Prometheus metrics + HEP RTCP chunk (verify forge-hep coverage).                                                                                  | **0.2.0**           |
| 8   | Multi-party AI conferences                              | ⚠️ Substantial upstream work                | Two-leg already works (B2BUA). 3+ party mixing needs a forge mixer + new protocol semantics.                                                                                                                                  | **0.3.0 / 0.4.0**   |
| 9   | Media-aware orchestration engine                        | n/a                                          | Vision, not a feature; delivered piecemeal across releases.                                                                                                                                                                  | Ongoing             |

### 3.2 Call handling — splitting by readiness

The roadmap lumps call control into one bucket. Real maturity differs sharply:

| Capability                              | Status today                                            | What's missing                                                       | Target                       |
| --------------------------------------- | ------------------------------------------------------- | -------------------------------------------------------------------- | ---------------------------- |
| Hold / resume                           | ✅ done                                                  | —                                                                    | shipped                      |
| Hangup                                  | ✅ done                                                  | —                                                                    | shipped                      |
| Blind transfer (REFER)                  | ✅ done                                                  | —                                                                    | shipped                      |
| Send DTMF                               | ✅ done                                                  | —                                                                    | shipped                      |
| Mute / unmute (sustained)               | ⚠️ partial (`Clear` flushes but no sustained mute)     | `BridgeIn::Mute` / `Unmute` + tap state                              | **0.2.0**                    |
| Call park / retrieve                    | ⚠️ park is a PBX feature, not really a bridge primitive | Either model as "transfer to parking orbit" or call out of scope     | **0.2.0 docs**               |
| Attended transfer (REFER with Replaces) | ❌ not implemented                                       | siphon-rs UAC needs Refer-To with Replaces; new dialog tracking      | **0.3.0** (likely upstream PR) |
| Conference (3+ party mixing)            | ❌ forge B2BUA = 2 legs only                             | Forge mixer + multi-leg manager in siphon-ai                         | **0.3.0 / 0.4.0**            |
| Whisper (private audio to one leg)      | ❌ not modelled                                          | Multi-leg audio routing in forge                                     | **0.3.0**                    |
| Supervisor barge                        | ❌                                                       | Same as whisper, plus 3-way mixing                                   | **0.4.0**                    |

The honest 0.2.0 call-handling delta is small: **add `Mute` / `Unmute` to the protocol, document park-as-transfer.** Attended transfer is the headline stretch.

## 4. Recommended 0.2.0 Scope (Must-Have)

Six well-scoped deliverables. Each is shippable on its own; together they materially expand what's possible without violating §4.1.

1. **Configurable call progress** — `[sip.call_progress]` block (`mode = instant_answer | ringing | session_progress`, optional early-media). siphon-rs already produces 100/180/183 via `IntegratedUAS`; sip-glue chooses based on config. Early media defers 200 OK and bridges RTP early — needs forge to allow pre-answer RTP send.
2. **Call handling protocol additions** — `BridgeIn::Mute` / `BridgeIn::Unmute` (sustained); document park-as-transfer; protocol version stays `1` (additive). One new SIPp scenario.
3. **RTP / media-quality events + metrics** — new `BridgeOut::RtpStats` event (default cadence ~5 s, configurable per-call). Prometheus histograms: `jitter_ms`, `packet_loss_ratio`, `rtcp_rtt_ms`. HEP RTCP chunk verified (forge-hep coverage).
4. **Media-primitive events** — `BridgeOut::SilenceDetected { duration_ms }` and `BridgeOut::DeadAirDetected { duration_ms }`, derived from forge-vad timing (no AI). Configurable thresholds.
5. **Twilio integration recipe** — `docs/INTEGRATIONS_TWILIO.md` (trunk config, public-IP / TLS, sample config block) + `examples/twilio-trunk/` with a minimal config that pairs with the existing echo-ws-server. *Not* a new bridge feature — adoption-side docs.
6. **Reference transcription example** — `examples/transcription-server-py/`, pure WS server consuming PCM frames and emitting Deepgram transcripts as events. Demonstrates the non-agent use case (high adoption leverage per the roadmap §3.2).

**Mandatory housekeeping per CLAUDE.md §4.5 / §4.6:**

- Each new event / config field documented in `docs/PROTOCOL.md` and `docs/CONFIG.md` *in the same PR*.
- Each new metric documented in `docs/DEPLOY.md`.
- **CI workflow finally added** (`.github/workflows/test.yml` — flagged as "recommended" pre-0.1.0; now blocking).

## 5. Recommended 0.2.0 Stretch (Slip Targets)

If the must-have list lands ahead of schedule, in order:

1. **WSS / SIP-TLS production recipe** — write `docs/DEPLOY_TLS.md` covering certbot rotation, mTLS for WSS (`reqwest` config; rustls peer-verifier), wire-format validation. No new code expected; docs + a tested deployment.
2. **Attended transfer (REFER with Replaces)** — depends on siphon-rs UAC support; will likely need a small upstream PR. Worth doing if siphon-rs is reasonably close.
3. **AI-provider-abstraction reference example** — `examples/provider-toolkit-py/` with pluggable Deepgram / Whisper STT, OpenAI / Anthropic / Groq LLM, ElevenLabs / Cartesia TTS. *No siphon-ai changes.* Lives entirely in `examples/`. Demonstrates the §4.1-respecting pattern end-to-end.

## 6. Deferred to 0.3.0 / 0.4.0 (with Reasons)

| Item                                                       | Why deferred                                                                                                                              |
| ---------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------- |
| SRTP                                                       | Real upstream work in forge-media; design considerations (DTLS-SRTP vs SDES); cert handling intersects with TLS work. Plan as own release. |
| mTLS / cert rotation / STIR-SHAKEN pass-through            | Enterprise-security cluster; coherent if shipped together.                                                                                |
| Attended transfer                                          | Wants siphon-rs UAC changes; defer unless 0.2.0 has spare capacity.                                                                       |
| Conference / mixing / whisper / barge                      | Forge must support N-leg mixing + per-leg routing. The biggest architectural lift on the roadmap.                                         |
| Live translation / supervisor / analytics examples         | Depend on whisper / multi-leg, or are big standalone projects.                                                                            |
| Semantic-event forwarding (`intent_detected`, etc.)        | Needs a clean "WS server → siphon-ai → webhook" path; small but better designed after the primitive events ship in 0.2.0.                 |

## 7. Sprint Plan (6 Weeks)

Modelled on the 7-week 0.1.0 plan; tighter because the foundation exists.

| Week | Focus                                  | Deliverables                                                                                                                          |
| ---- | -------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------- |
| 1    | Call progress + protocol additions     | `[sip.call_progress]` config + sip-glue plumbing; `BridgeIn::Mute` / `Unmute`; doc updates in `PROTOCOL.md` / `CONFIG.md`; one new SIPp scenario |
| 2    | Media-primitive events                 | `silence_detected` / `dead_air_detected` from forge-vad timing; configurable thresholds; event docs + metrics                          |
| 3    | RTP stats                              | `BridgeOut::RtpStats` event; periodic emitter task; Prometheus histograms; HEP RTCP chunk verification                                 |
| 4    | Twilio recipe + CI workflow            | `docs/INTEGRATIONS_TWILIO.md`; `examples/twilio-trunk/`; `.github/workflows/test.yml` (cargo build / test / clippy / fmt + SIPp suite) |
| 5    | Transcription reference example        | `examples/transcription-server-py/`; Deepgram + AssemblyAI variants; deployment doc                                                    |
| 6    | Hardening + release                    | Full smoke test, SIPp suite green, CHANGELOG, version bump, tag                                                                       |

Stretch items slot into spare time, in §5 order. If stretch eats more than Week 5, bump them to 0.2.1.

## 8. Protocol Versioning

All proposed 0.2.0 additions are **additive** to the v1 WS protocol — new `BridgeIn` / `BridgeOut` variants and new event types. WS servers built against v1 continue to work; they ignore unknown messages.

Therefore: **protocol stays at `version: "1"` for 0.2.0.** A v2 bump would only be needed if we *change* an existing message shape (e.g. dropping a field). Don't do that for this release.

## 9. Decisions Before Sprint 1

These are the gates worth resolving before any code lands. The recommendations are opinions, not facts:

1. ✅ **Early media in `session_progress` mode** — **resolved**.
   - **Forge**: investigated — supports it today, no changes needed. The acceptor already calls `start_session()` *before* `accept_invite` (see `crates/core/src/acceptor.rs:1331`, with an explicit "Start forge's RTP forwarding loop BEFORE the 200 OK" comment), and `send_rtp_to` has no "answered" gate, only requiring the remote address (already seeded from the offer).
   - **siphon-rs**: was missing one RFC 3262 correctness fix — `create_reliable_provisional` ignored the dialog's local tag and stamped a random one, so PRACK matching would never work end-to-end. Fixed upstream in [siphon-rs#47](https://github.com/thevoiceguy/siphon-rs/pull/47); siphon-ai bumped to that rev.
   - **Decision**: 0.2.0 ships **Flavour B (best-effort 183 with negotiated SDP)**. Peers that include `Require: 100rel` in the INVITE are detected at INVITE time and the call falls through to `instant_answer` with a `warn!` log (so they still get bridged, just without reliable provisionals). The reliable / 100rel-honouring variant — and the "AI plays a prompt during the 183 phase" *Flavour C* — are deferred to **0.2.1 / 0.3.0**, when `on_prack` wiring in `RoutingHandler` and a `BridgeIn::Answer` control message are tackled together.
2. **`silence_detected` / `dead_air_detected` default thresholds** — proposal: `silence_threshold_ms = 1500`, `dead_air_threshold_ms = 5000`. Survey against actual call traffic before locking.
3. **RTP stats cadence** — default emit interval? Proposal: `5000 ms`. Configurable per-call via a control message?
4. **Twilio example scope** — inbound-trunk config + pointer to echo-ws-server, or a fuller "Twilio sends webhook, siphon-ai handles the SIP" loop? The former is much smaller.
5. **Transcription example STT provider** — Deepgram only (faster) or Deepgram + AssemblyAI + Whisper (toolkit pattern)? The latter doubles as a partial answer to "provider abstraction" without putting it in the bridge.
6. **Park semantics** — pure docs ("model as REFER to parking orbit") or a thin `BridgeIn::Park { orbit }` helper that wraps `Transfer` with the right URI? Recommendation: doc route — park is a PBX feature.

## 10. Definition of Done — v0.2.0

A reasonable user can:

1. Configure call-progress behaviour per deployment (instant-answer / ringing / session-progress).
2. Receive `silence_detected` and `dead_air_detected` events in their WS server.
3. Receive periodic `rtp_stats` events with jitter, packet loss, and RTCP RTT.
4. Mute and unmute the AI's playout independently of `Clear` / barge-in.
5. Stand up a Twilio SIP trunk against SiphonAI in under an hour using the integration doc.
6. Clone the transcription example and have a working live-transcription pipeline against a SIP call in under 15 minutes.
7. See a CI pipeline gate every PR on build, test, clippy, and fmt.
8. Operate a TLS-secured deployment (SIP/TLS + WSS) using the existing 0.1.0 mechanics with the new deployment recipe.

Acceptance bar same as 0.1.0 §11.8 — every new event / feature must answer the "can I diagnose this from logs / metrics / traces / HEP alone?" test.

---

This plan trims the 0.2.0 roadmap to what siphon-ai can ship in ~6 weeks while staying true to §4.1, and pushes the heavier items (SRTP, conferences, attended transfer, semantic events) into 0.3.0 / 0.4.0 where they can be designed properly. The transcription example and Twilio docs are especially high-leverage — they reframe the project as a *platform*, not just a voice-agent bridge.
