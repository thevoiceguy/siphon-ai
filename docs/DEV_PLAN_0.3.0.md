# SiphonAI 0.3.0 Development Plan

**Theme: trust & encryption — secure-on-the-wire end-to-end.**

0.1.0 shipped the bridge. 0.2.0 shipped the operator primitives.
0.3.0 makes the wire itself defensible — SRTP media (SDES + DTLS-SRTP),
mTLS on the bridge WS leg, hot-reload of the SIP/TLS cert without
dropping calls, REGISTER over TLS, and the `rtcp_rtt_ms` field
that's been promised since 0.2.0.

## 1. Cardinal Rule, Restated

**Still no AI code.** Same line as 0.1.0 / 0.2.0. Encryption is a
wire-level concern; nothing in this release changes the
"WS server runs the AI" architecture. `forge-ai-stream` remains
not-depended-upon. (See `CLAUDE.md` §4.1.)

## 2. Already Shipped (0.1.0 + 0.2.0)

Both these are settled and stay settled. Do not redo:

- 0.1.0: SIP signaling, RTP bridging, WS protocol v1, routing, CDR, webhooks, HEP, admin/health, register-source mode, Docker/systemd packaging.
- 0.2.0: silence/dead-air events, RTP stats event, mute/unmute, configurable call progress (instant/ringing/session-progress Flavour B), Twilio recipe, transcription reference, CI gate, TLS deployment recipe.

Both 0.1.0 and 0.2.0 listed SRTP and mTLS as explicit non-goals. The
0.2.0 plan §6 paired them with cert rotation as "Enterprise-security
cluster; coherent if shipped together." This release is that cluster.

## 3. Item-by-Item Disposition of 0.2.1 / 0.3.0 Backlog

The 0.2.0 CHANGELOG `Deferred to 0.2.1` and the DEV_PLAN_0.2.0.md §6
`Deferred to 0.3.0 / 0.4.0` lists are the source of truth. Status
re-evaluated against an upstream survey of forge-media + siphon-rs
(May 2026):

| Item | Survey finding | 0.3.0 decision |
|---|---|---|
| **SRTP** | forge-media has full RFC 3711 + RFC 5764 DTLS-SRTP (PR #24, AES-CM + AES-GCM, benchmarked). Does **not** have SDES (`a=crypto:`). siphon-rs SDP parses `RTP/SAVP` + `UDP/TLS/RTP/SAVPF` + `a=fingerprint`. | **§4 must-have.** SDES + DTLS-SRTP both. SDES needs one upstream forge PR; DTLS-SRTP wires as-is. |
| **mTLS for bridge WS leg** | tokio-tungstenite + rustls already support client certs; just no config surface in siphon-ai. No upstream work. | **§4 must-have.** siphon-ai-only. |
| **Hot SIP/TLS cert reload** | siphon-rs `TlsConfig` is immutable Arc; no swap path. | **§4 must-have.** Small siphon-rs PR makes `ServerConfig` swappable via `ArcSwap`. |
| **`rtp_stats.rtcp_rtt_ms`** | forge-media has the RTCP receiver but doesn't surface RTT on the Quality / RtpStats event. | **§4 must-have.** Small forge PR exposes RTT via the existing event surface. |
| **Outbound TLS UAC for REGISTER** | siphon-rs has `send_tls()` + DNS transport selection + pooling. 0.2.0 CONFIG.md note ("Inbound UAS only") is stale. | **§4 must-have.** Zero upstream work; siphon-ai config + glue + REGISTRATION.md TLS section. |
| **Attended transfer (REFER with Replaces)** | siphon-rs parses `Replaces`; UAC doesn't *generate* `Refer-To: …;Replaces`. | **§5 stretch.** Small siphon-rs PR + `BridgeIn::AttendedTransfer` variant. |
| **`examples/provider-toolkit-py/`** | Carried from 0.2.0 §5.3 slip. No upstream dependency. | **§5 stretch.** Pure examples work, no bridge change. |
| **STIR/SHAKEN pass-through** | No upstream work — no Identity-header parsing in siphon-rs at all. | **§6 deferred to 0.4.0.** Real siphon-rs work; coherent with a richer auth release. |
| **Recording** | Unchanged from CLAUDE.md §8. | **§6 deferred.** Post-v1 by policy. |
| **Conferencing / mixing** | Unchanged. | **§6 deferred.** Biggest forge lift on the roadmap. |
| **WS reconnect mid-call** | Unchanged. | **§6 deferred.** |
| **Semantic event forwarding** | Unchanged. | **§6 deferred.** Better-designed after a primitive-events soak. |

## 4. Recommended 0.3.0 Scope (Must-Have)

Five deliverables. Each shippable on its own; together they make
"deployed siphon-ai is encrypted end-to-end" a true statement.

1. **SRTP media — SDES + DTLS-SRTP.**
   - **Upstream**: forge-media PR adding SDES (RFC 4568) — `a=crypto:` parse/generate + key-derivation glue on top of the existing `srtp.rs` crypto primitives. DTLS-SRTP needs no upstream change.
   - **siphon-ai**: SDP answer-shape logic that inspects the offer's `m=` profile (`RTP/AVP` / `RTP/SAVP` / `UDP/TLS/RTP/SAVPF`) and emits the matching answer. Negotiation in `crates/core/src/acceptor.rs`. Media-glue forwards the SRTP context (master key + salt for SDES; DTLS fingerprint for DTLS-SRTP) to forge-rtp.
   - **Config**: `[media].srtp = "off" | "preferred" | "required"`.
     - `"off"` (default) — answer plaintext only. 488 if offer is `RTP/SAVP` and `"off"` is set explicitly (no silent downgrade).
     - `"preferred"` — answer SRTP if offered, plaintext otherwise.
     - `"required"` — 488 if offer is `RTP/AVP`.
   - Per-route override (`[route.media].srtp`).
   - SIPp scenarios: `srtp_sdes_then_bye.xml`, `srtp_dtls_negotiate.xml` (where SIPp can drive DTLS — likely a fixture limitation, document if so).
   - **PROTOCOL impact**: new optional `srtp: { profile: "AES_CM_128_HMAC_SHA1_80" | …, exchange: "sdes" | "dtls" }` field on the `start` message so the WS server knows the call is encrypted. Additive; protocol stays v1.

2. **mTLS for the bridge WebSocket leg.**
   - **siphon-ai-only.** No upstream PR.
   - **Config**:
     ```toml
     [bridge.tls]
     client_cert    = "/etc/siphon-ai/bridge/client.pem"  # PEM chain
     client_key     = "/etc/siphon-ai/bridge/client.key"  # PEM private key
     pinned_sha256  = "…"   # optional — single SPKI pin, exact match required when set
     ```
   - Builds a custom `rustls::ClientConfig` for the bridge connection; tokio-tungstenite accepts it via `connect_async_tls_with_config`. Pin verification piggybacks on a custom `ServerCertVerifier` when `pinned_sha256` is set.
   - Per-route override (`[route.bridge.tls]`).
   - Test: integration test against a WS server that requires a client cert; an integration test with a wrong pin should fail closed.
   - **DEPLOY.md**: extend § TLS deployment with an mTLS subsection.

3. **Hot SIP/TLS cert reload (SIGHUP).**
   - **Upstream**: siphon-rs PR wraps `TlsConfig`'s inner `ServerConfig` in `ArcSwap<ServerConfig>`. Accept loop reads the current swap on each new TLS connection — in-flight TLS sessions keep using the cert that handshook them; new sessions use the swapped-in cert.
   - **siphon-ai**: SIGHUP handler re-reads `[sip.tls].cert` + `.key` from disk, builds a new `ServerConfig`, calls `tls_config.swap(new)`. No call drop. Logs `info!` on success, `error!` + keep-old-cert on failure (broken PEM file shouldn't kill the daemon).
   - **DEPLOY.md**: replace the existing "restart on renewal" Let's Encrypt deploy-hook with `systemctl reload` (which sends SIGHUP via the unit file). Document old restart path as fallback.
   - Test: integration test that swaps a cert mid-flight and asserts existing TLS dialogs survive + new dialogs use the new cert.

4. **`rtp_stats.rtcp_rtt_ms` populated.**
   - **Upstream**: forge-media PR surfaces RTCP-derived RTT on the Quality / RtpStats event surface. Implementation: track sent-RR timestamp + matching SR's `LSR` / `DLSR` (RFC 3550 §6.4.1, §A.7) inside forge-rtp; expose `rtt_ms: Option<f32>` on the Quality event the same way `jitter_ms` is exposed.
   - **siphon-ai**: cache the new field in `RtpStatsTracker` (already a cache-and-emit shape); populate `BridgeOut::RtpStats.rtcp_rtt_ms` (already-reserved field per PROTOCOL §3.8); add `siphon_ai_rtp_rtt_ms` Prometheus histogram.
   - No new config — uses existing `[bridge].rtp_stats_interval_ms`.

5. **Outbound TLS UAC for REGISTER.**
   - **siphon-ai-only.** siphon-rs `send_tls()` already covers the transport.
   - **Config**: `[[register]].server` accepts `sip:host;transport=tls`. New optional `[[register]].tls` block with `pinned_sha256` if cert pinning is wanted (carrier-pinned PBX scenarios).
   - DNS resolution: respect RFC 3263 `_sips._tcp` SRV when transport is TLS.
   - **REGISTRATION.md**: TLS section with cert source / pinning / Twilio example.
   - **CONFIG.md fix**: remove the stale "Inbound UAS only" disclaimer; replace with a precise statement of what's UAS-only vs UAC-supported now.

**Mandatory housekeeping per CLAUDE.md §4.5 / §4.6:**

- Each new config field documented in `docs/CONFIG.md` in the same PR.
- Each new metric (`siphon_ai_rtp_rtt_ms`) documented in `docs/DEPLOY.md` Metrics section.
- Each new SIPp scenario wired into `test-harness/sipp-scenarios/run-all.sh` (the lesson from PR #80 — files-without-wiring don't count as gated tests).
- PROTOCOL.md additions for `start.srtp` field.

## 5. Recommended 0.3.0 Stretch (Slip Targets)

In order. If any of these eats more than a half-week beyond their slot, push to 0.3.1.

1. **Attended transfer (REFER with Replaces).** Upstream siphon-rs PR for `Refer-To: …;Replaces=<call-id>;to-tag=…;from-tag=…` generation in UAC. siphon-ai: `BridgeIn::AttendedTransfer { call_id, replaces_call_id }` variant, dispatched same as the existing `BridgeIn::Transfer`. New SIPp scenario.

2. **`examples/provider-toolkit-py/`.** Pluggable Deepgram/Whisper STT + OpenAI/Anthropic/Groq LLM + ElevenLabs/Cartesia TTS reference. Lives entirely in `examples/`. No siphon-ai changes. The 0.2.0 §5.3 slip, carried.

## 6. Deferred to 0.4.0+ (with Reasons)

| Item | Why deferred |
|---|---|
| STIR/SHAKEN pass-through / verification | No siphon-rs Identity-header parsing today; significant upstream work. Coherent with a richer call-auth release (CALLER-ID attestation + verstat). |
| STIR/SHAKEN attestation generation | Even bigger — needs cert issuance, JWT signing, OCSP integration. |
| Recording | CLAUDE.md §8 post-v1. Forge + storage abstraction + privacy controls. |
| Conferencing / mixing / whisper / barge | Largest architectural lift; needs N-leg per-call routing in forge. |
| WS reconnect mid-call | CLAUDE.md §8 post-v1. Tricky around stale `seq` numbers and audio re-sync. |
| Semantic event forwarding (`intent_detected`, etc.) | Better designed after the primitive-events soak from 0.2.0 finishes. |
| SDES outbound (we call out, the carrier expects SDES) | Predicated on outbound-INVITE origination, which is itself post-v1. |

## 7. Sprint Plan (6 Weeks)

Three upstream PRs sit in the critical path. Open them all in Week 1
so review/back-pressure runs in parallel with siphon-ai work that
doesn't need them yet.

| Week | Focus | Deliverables |
|---|---|---|
| 1 | Upstream PRs + SRTP scaffolding | Open forge-media SDES PR + forge-media RTCP-RTT PR + siphon-rs swappable-TLS PR. siphon-ai: `[media].srtp` config surface; `srtp` enum on `start` message; SRTP context types in media-glue. No wire behaviour yet. |
| 2 | DTLS-SRTP wiring (no upstream gate) | Answer-path for `UDP/TLS/RTP/SAVPF` offers; media-glue forwards DTLS fingerprint + key material to forge-rtp; first SIPp scenario (`srtp_dtls_negotiate.xml`, marked `#[ignore]` if SIPp can't drive DTLS). |
| 3 | SDES wiring + RTT wiring | Conditional on the forge PRs landing. Answer-path for `RTP/SAVP` offers; SDES key derivation glue; `rtcp_rtt_ms` populated in `RtpStatsTracker` + histogram. `srtp_sdes_then_bye.xml` scenario. |
| 4 | mTLS for bridge WS + outbound TLS UAC REGISTER | Parallel — no shared code. `[bridge.tls]` config; rustls connector wiring; cert-pinning verifier. Register-over-TLS config + DNS + REGISTRATION.md section. |
| 5 | Hot SIP/TLS cert reload | Conditional on siphon-rs PR landing. SIGHUP handler; `tls_config.swap(new)`; integration test for mid-flight cert swap. DEPLOY.md SIGHUP-based renewal. |
| 6 | Hardening + release | Full smoke test, SIPp suite green incl. TLS scenarios, CHANGELOG, version bump, tag, GitHub release. Twilio-trunk real-world validation if a test trunk is available. |

Stretch items slot into spare time, in §5 order. If a stretch eats
more than Week 6, bump to 0.3.1.

**Upstream PR slip mitigation**: each upstream PR has a fallback —

- forge SDES PR slips → ship 0.3.0 with **DTLS-SRTP only** and re-flag SDES for 0.3.1. Document the decision in CHANGELOG.
- forge RTT PR slips → ship 0.3.0 with `rtcp_rtt_ms` still `null` (matches current behaviour); 0.3.1 follow-up.
- siphon-rs swappable-TLS PR slips → ship 0.3.0 with restart-only cert renewal (matches 0.2.0); 0.3.1 follow-up.

None of the slips block the release — they reduce scope. That's why
all three upstream PRs are Week 1.

## 8. Protocol Versioning

All 0.3.0 additions are **additive** to v1:

- `BridgeOut::Start.srtp` is a new optional field (omitted in v1 servers' message structure when SRTP is off, present when on).
- `BridgeOut::RtpStats.rtcp_rtt_ms` is already-reserved per PROTOCOL §3.8; the change is "now populated" not "new field."
- `BridgeIn::AttendedTransfer` (stretch §5.1) is a new variant.

**Protocol stays at `version: "1"` for 0.3.0.** Servers built against
0.1.0 / 0.2.0 keep working unchanged; they ignore the new `srtp`
field on `start` and the new `BridgeIn::AttendedTransfer` variant.

## 9. Decisions Before Sprint 1

Most decisions resolved during the user-facing planning conversation.
Recorded here for reference and so a fresh contributor isn't asked to
re-decide:

1. ✅ **SRTP key exchange** — **resolved**. Ship both SDES + DTLS-SRTP. Driven by the need to interop with both classic SIP carriers (Twilio Elastic SIP Trunk's Secure Media is SDES) and WebRTC bridges (DTLS-SRTP).
2. ✅ **`[media].srtp` default** — **resolved**. Default to `"off"` for backwards compatibility — no 0.2.0 → 0.3.0 upgrade can break a working trunk by accident. Operators who want SRTP set `"preferred"` or `"required"` explicitly.
3. ✅ **Outbound TLS UAC scope for 0.3.0** — **resolved**. REGISTER only. Originating outbound INVITEs is post-v1 per CLAUDE.md §8 ("Outbound originated calls"); enabling outbound TLS just for REGISTER closes the obvious gap without changing call-direction scope.
4. ✅ **mTLS cert pinning shape** — **resolved**. Stick with default rustls CA-store verification (`webpki-roots`) plus optional single-cert SPKI pin via `[bridge.tls.pinned_sha256]`. Multi-cert pin sets and pin rotation are 0.3.1/0.4.0 work.
5. ✅ **Hot cert reload trigger** — **resolved**. SIGHUP. Standard Unix idiom, no inotify dependency, works under systemd `systemctl reload`. Revisit if operators ask for file-watch behaviour.
6. ✅ **RTCP RTT scope in forge PR** — **resolved**. Mean RTT across the `rtp_stats_interval_ms` window, derived from RTCP RRs per RFC 3550 §A.7. Per-packet RTT is out of scope; per-window mean lines up with the existing rtp_stats cadence.
7. ✅ **Sprint length** — **resolved**. 6 weeks. Three coordinated upstream PRs make a shorter sprint unrealistic; 8 weeks would be more comfortable but the slip-mitigation in §7 lets 6 work.

## 10. Definition of Done — v0.3.0

A reasonable user can:

1. Enable SRTP against a Twilio Elastic SIP Trunk (SDES path) and complete a call end-to-end with audio in both directions.
2. Bridge a WebRTC-side caller (DTLS-SRTP) via a SIP-WebRTC gateway and complete a call end-to-end.
3. Connect to an mTLS-protected WebSocket server using a client cert pinned by SPKI hash.
4. Rotate the SIP/TLS cert via `systemctl reload siphon-ai` without dropping any in-flight TLS calls.
5. Register an inbound trunk over `sip:host;transport=tls` to a TLS-only PBX.
6. See `rtcp_rtt_ms` populated in `rtp_stats` events on every active call (when RTCP is flowing).
7. CI gates every PR on build + clippy + fmt + cargo test + SIPp suite (now including TLS/SRTP scenarios). Carries forward from 0.2.0.
8. Upgrade from 0.2.0 to 0.3.0 with no config changes and observe no behavioural difference — every new feature defaults to off / status-quo.

## 11. Risks

Listed once, briefly:

- **Upstream PR review latency.** Three PRs, one author shared with this repo. Mitigation: §7 slip plan.
- **SDES key-exchange security profile.** SDES exchanges the SRTP master key over the SIP signaling plane — if signaling is plaintext UDP, the key is in the clear and SRTP gives nothing. Document loudly: `srtp = "preferred"` over plaintext SIP is a footgun, not protection. Pair `[sip.tls]` with `[media].srtp` in the recipe.
- **DTLS-SRTP SIPp coverage.** SIPp doesn't natively drive DTLS-SRTP. Either accept that the DTLS path is tested by hand against a real WebRTC bridge, or invest in a Rust-side integration test that wires forge-rtp's DTLS-SRTP loopback directly. Decision: hand-test for 0.3.0, file an issue for the Rust harness.
- **mTLS pin rotation UX.** Pin-by-hash means cert rotation needs a config edit, not just a renewal. Document this; mention multi-pin support is 0.3.1+.
