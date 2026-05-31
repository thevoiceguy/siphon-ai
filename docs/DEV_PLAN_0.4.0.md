# SiphonAI 0.4.0 Development Plan

**Theme: call authentication — every inbound call gets a trust verdict.**

0.1.0 shipped the bridge. 0.2.0 shipped the operator primitives.
0.3.0 made the wire itself defensible (SRTP, mTLS, hot-reloadable
SIP/TLS cert, REGISTER over TLS). 0.4.0 turns the page on
**who's actually calling** — STIR/SHAKEN verification of every
inbound INVITE, with the verdict surfaced into the WS protocol,
the CDR, and HEP so operators can build fraud policy on top.

## 1. Cardinal Rule, Restated

**Still no AI code.** Same line as 0.1.0 / 0.2.0 / 0.3.0.
STIR/SHAKEN is a SIP-layer signaling concern; the WS server
remains where any AI / business logic lives. `forge-ai-stream`
remains not-depended-upon. (See `CLAUDE.md` §4.1.)

## 2. Already Shipped (0.1.0 + 0.2.0 + 0.3.0)

Settled and stay settled. Do not redo:

- 0.1.0: SIP signaling, RTP bridging, WS protocol v1, routing, CDR, webhooks, HEP, admin/health, register-source mode, Docker/systemd packaging.
- 0.2.0: silence/dead-air events, RTP stats event, mute/unmute, configurable call progress (instant/ringing/session-progress Flavour B), Twilio recipe, transcription reference, CI gate, TLS deployment recipe.
- 0.3.0: SRTP media (SDES + DTLS-SRTP), mTLS for bridge WS leg, hot SIP/TLS cert reload (SIGHUP), REGISTER over TLS, `rtcp_rtt_ms` reserved + wiring scaffold.

0.3.0 listed STIR/SHAKEN pass-through and attestation as explicit
deferrals to 0.4.0+. The 0.3.0 plan §6 grouped them as "a richer
call-auth release coherent if shipped together." This release is
that cluster.

## 3. Item-by-Item Disposition of 0.3.1 / 0.4.0 Backlog

The 0.3.0 CHANGELOG `Known limitations (0.3.1 carry-forwards)`
section and DEV_PLAN_0.3.0.md §6 `Deferred to 0.4.0+` are the
source of truth. Status re-evaluated against an upstream survey
of forge-media + siphon-rs:

| Item | Survey finding | 0.4.0 decision |
|---|---|---|
| **STIR/SHAKEN pass-through (inbound verstat surfacing)** | No upstream work — no Identity-header parsing in siphon-rs at all. PASSporT (RFC 8225) is JWT-shaped, so the Rust ecosystem is rich (`jsonwebtoken`, `ring`); cert-chain validation against STI-PA roots is `rustls-webpki`-shaped. | **§4 must-have.** Sizeable siphon-rs PR for Identity parsing + verification primitive; siphon-ai owns the policy + surfacing. |
| **STIR/SHAKEN verification (signature + cert chain + orig/dest match)** | Same upstream PR as pass-through — verification is the *value* of the parsing. Pass-through without verification is just header echoing. | **§4 must-have.** Verification IS the feature; surfacing the verdict is the operator API. |
| **Verstat policy gate** (`[security].min_attestation`) | Pure siphon-ai. Reject inbound calls below a configured attestation threshold with 4xx. | **§4 must-have.** Defining feature for "the operator can do something about it." |
| **Admin endpoint for verstat history** | Pure siphon-ai. `/admin/calls/{id}/verstat` + last-N rolling buffer. | **§5 stretch.** Diagnostic ergonomics on top of the core verification path. |
| **Semantic event forwarding** (`intent_detected`, etc.) | Carried from 0.3.0 §6. Better-designed after the 0.2.0 primitive-events soak in production. | **§5 stretch.** Independent of STIR/SHAKEN, fits if there's headroom in the sprint. |
| **STIR/SHAKEN attestation generation (we sign)** | Needs STI-CA cert enrollment (multi-week real-world process), JWT signing with ES256, Identity-header injection on outbound. AND it's predicated on outbound-INVITE origination, which is itself post-v1. | **§6 deferred to 0.5.0.** Wrong release — depends on a feature that doesn't exist yet. |
| **Outbound originated INVITEs** | Carried from CLAUDE.md §8. Fundamentally changes auth, dialog ownership, and SIP routing model. | **§6 deferred.** Independent post-v1 work that unlocks several other features. |
| **Recording** | Carried from 0.3.0 §6. forge + storage abstraction + privacy controls. | **§6 deferred.** Post-v1 by policy. |
| **Conferencing / mixing / whisper / barge** | Carried from 0.3.0 §6. Biggest architectural lift on the roadmap. | **§6 deferred.** Post-v1; needs N-leg per-call routing in forge. |
| **WS reconnect mid-call** | Carried from 0.3.0 §6. | **§6 deferred.** Sequence + audio-resync work; tricky in isolation. |

## 4. Recommended 0.4.0 Scope (Must-Have)

Three deliverables. The first depends on the upstream siphon-rs PR;
the second and third are siphon-ai-only and can ship the moment
the parsing primitive lands.

1. **Identity header parsing + PASSporT verification.**
   - **Upstream**: siphon-rs PR adding `sip-identity` crate. Parses RFC 8224 `Identity:` header (`<base64>;info=<x5u>;alg=ES256;ppt=shaken`), decodes the PASSporT JWT (RFC 8225) — header + payload + signature — fetches the signing cert from the `info=` URL with an in-memory TTL cache, validates the cert chain against a configurable trust store (default: iconectiv STI-PA roots), and exposes a `verify(invite) → VerificationResult` API. Verification covers: signature, cert chain, `orig`/`dest` claim numbers matching SIP From/To E.164 forms, `iat` freshness window, `ppt=shaken` extension claims (`attest`, `origid`).
   - **siphon-ai**: The verification primitive runs as a step in the INVITE accept path, before route matching. Result is stored on the per-call state for downstream surfacing. **No** policy enforcement at this layer — surfacing + observability only; the policy gate (item 3) is the layer that decides what to do with the verdict.
   - **Result shape**: `VerificationResult { attest: Option<AttestationLevel>, origtn: Option<String>, origtn_passed: bool, dest_passed: bool, cert_chain_valid: bool, signature_valid: bool, error: Option<String> }`. The five booleans + the error string let downstream consumers reconstruct the precise failure mode without re-parsing.
   - **Config**: `[security.stir_shaken]` block, default off (zero behaviour change for 0.3.0 deployments).
     ```toml
     [security.stir_shaken]
     enabled         = true
     trust_anchors   = "/etc/siphon-ai/sti-pa-roots.pem"
     cert_cache_ttl  = "1h"
     # When the Identity header is absent on an inbound INVITE,
     # the resulting verdict is `attest: null, signature_valid: false`.
     # `require_identity = true` rejects unsigned calls with 428
     # ("Use Identity Header", RFC 8224 §6.2.2).
     require_identity = false
     ```

2. **Surface the verdict into the WS protocol, CDR, and HEP.**
   - **PROTOCOL**: new optional `verstat` object on `BridgeOut::Start`:
     ```json
     {
       "type": "start",
       "call_id": "...",
       "verstat": {
         "attest": "A",
         "orig_tn": "+15183217034",
         "orig_passed": true,
         "dest_passed": true,
         "cert_chain_valid": true,
         "signature_valid": true,
         "error": null
       },
       ...
     }
     ```
     Field omitted entirely when `[security.stir_shaken].enabled = false`. v1 servers that don't know the field ignore it.
   - **CDR**: new optional fields `verstat_attest: Option<String>` (`"A"`/`"B"`/`"C"`/`null`) and `verstat_passed: Option<bool>` (composite of `signature_valid && orig_passed && dest_passed && cert_chain_valid`). Additive — CDR schema version stays at 1.
   - **HEP**: emit a vendor-specific HEP3 chunk on accept carrying the full verdict, so Homer can correlate STIR/SHAKEN outcomes against the SIP dialog and RTP QoS already shipped per 0.3.0 §4.
   - **Logging**: one `info!` line per accepted call with `verstat_attest=A verstat_passed=true orig_tn=...`. Operators grepping the journal for fraud patterns get a clean field-keyed line.

3. **Verstat policy gate (`[security].min_attestation`).**
   - Pre-route policy check. After verification, before route matching: if the call's attestation is below `min_attestation`, reject with 4xx and a `Reason:` header explaining why.
   - **Config**:
     ```toml
     [security]
     min_attestation = "B"   # "A" | "B" | "C" | "none" (default: "none")
     min_attestation_response = 403   # 403 | 488 | 606 (default: 403)
     ```
   - Policy matrix:

     | `min_attestation` | A | B | C | none (header absent) | invalid signature |
     |---|---|---|---|---|---|
     | `"none"` | ✓ | ✓ | ✓ | ✓ | ✓ |
     | `"C"` | ✓ | ✓ | ✓ | reject | reject |
     | `"B"` | ✓ | ✓ | reject | reject | reject |
     | `"A"` | ✓ | reject | reject | reject | reject |

   - **Per-route override**: `[route.security].min_attestation` overrides the global. A route handling high-trust traffic (internal IVR) might lock to `"A"`; a route handling consumer inbound might allow `"C"`. Same resolution helper pattern as `resolve_srtp_mode` from 0.3.0.
   - **Metric**: `siphon_ai_invites_total{result="rejected_attestation"}` ticks for every gate-rejected call. Alertable.

## 5. Recommended 0.4.0 Stretch (Slip Targets)

In order. If any of these eats more than a half-week beyond its
slot, push to 0.4.1.

1. **`/admin/calls/{id}/verstat` admin endpoint** — diagnostic
   surface; reads the per-call verdict stored under §4-item-1.
   Plus a rolling `/admin/verstat/recent` last-N buffer for
   fraud-investigation workflows. ~1 week.
2. **Semantic event forwarding** — `BridgeOut::IntentDetected`,
   `BridgeOut::EntityExtracted` etc., emitted by the WS server's
   business layer and ferried back over the bridge to the WS
   server itself (or to a sibling consumer). Carried from
   0.3.0 §6. Independent of STIR/SHAKEN; fits if there's
   headroom. ~1.5 weeks.
3. **Twilio Caller Identity recipe** — most operators will
   STIR/SHAKEN-verify inbound from Twilio; Twilio sets
   `X-Twilio-VerStat: TN-Validation-Passed-A` already (we saw it
   in production traces). The recipe shows how to compare
   Twilio's verstat header against our own verification result —
   useful as a sanity check during initial deployment and as
   documentation that "the verdict from STIR/SHAKEN matches what
   the carrier already told us." ~0.5 week.

## 6. Deferred to 0.5.0+ (with Reasons)

| Item | Why deferred |
|---|---|
| **STIR/SHAKEN attestation generation** (we sign outbound) | Predicated on outbound-INVITE origination, which is itself post-v1. Also needs STI-CA cert enrollment which is a multi-week real-world process per operator. Right release: whichever one ships outbound-origination. |
| **Outbound originated calls** | CLAUDE.md §8 post-v1. Fundamentally changes auth, dialog ownership, and SIP routing model. |
| **Multi-trust-anchor verstat policy** (e.g. "accept A from anchor X, only A from anchor Y") | Probably never needed — the iconectiv STI-PA root is the authoritative anchor for US numbers. Defer unless multi-jurisdiction deployments ask for it. |
| **Verstat caching across calls from the same originator** | The `info=` cert cache already amortises the expensive part (cert fetch + chain validation). Per-originator verdict caching would mean accepting that a call's verstat might be stale by minutes — wrong trade-off for fraud control. Defer indefinitely. |
| **Recording** | CLAUDE.md §8 post-v1. Unchanged from 0.3.0 §6. |
| **Conferencing / mixing / whisper / barge** | Unchanged. Biggest architectural lift on the roadmap. |
| **WS reconnect mid-call** | Unchanged. |
| **Inline transcription (no WS server bridge)** | Architectural conflict with the "WS server runs the AI" rule. Always post-v1. |

## 7. Sprint Plan (6 Weeks)

One upstream PR sits in the critical path. Open it in Week 1 so
review/back-pressure runs in parallel with siphon-ai work that
doesn't need it yet.

| Week | Focus | Deliverables |
|---|---|---|
| 1 | Upstream PR + scaffolding | Open siphon-rs `sip-identity` PR (Identity header parser + PASSporT JWT verifier + STI-PA trust-store loader). siphon-ai: `[security.stir_shaken]` config surface; `VerificationResult` types in a new `crates/security/` crate; trust-anchor file plumbing. No wire behaviour yet. |
| 2 | Surface plumbing (no upstream gate) | `verstat` field on `BridgeOut::Start`; CDR `verstat_attest` / `verstat_passed` fields with schema-version bump-OR-stay decision; per-call state slot for the verdict. Tests with synthetic `VerificationResult` values to exercise the surface without needing the upstream parser. |
| 3 | Wire the verifier in | Conditional on upstream PR landing. INVITE accept path calls into the verifier before route matching; verdict lands on per-call state and threads through to the surfaces wired in W2. First SIPp scenario (`stir_shaken_attest_a.xml`) — pre-recorded PASSporT JWT against a test cert. |
| 4 | Policy gate + per-route override | `[security].min_attestation`; `[route.security].min_attestation` override; 4xx rejection path with `Reason:` header; metric ticks; per-policy unit tests for the matrix in §4-item-3. |
| 5 | HEP + observability + reference recipe | Vendor-specific HEP chunk emission on accept; structured log line; `Twilio Caller Identity` recipe (stretch item 3 promoted into core if W1-4 were tidy). |
| 6 | Hardening + release | Full smoke test, SIPp suite green incl. STIR/SHAKEN scenarios, CHANGELOG, version bump, tag, GitHub release. Real-world validation against a Twilio inbound (which always carries `Identity:` from US-originated calls). |

Stretch items slot into spare time, in §5 order. If a stretch
eats more than Week 6, bump to 0.4.1.

**Upstream PR slip mitigation**: the siphon-rs `sip-identity`
PR is the only critical-path dependency. If it slips past Week 3:

- ship 0.4.0 with **pass-through only** — Identity header *parsed*
  and surfaced as-is into the `verstat` field (without verification),
  flagged in the protocol field as `signature_valid: null` rather
  than `false`. Operators can still see the carrier's attestation
  claim, just can't trust it. Document the limitation.
- re-flag verification + policy gate for 0.4.1. Pass-through alone
  is still a real shipped feature (it surfaces what Twilio already
  tells us in `X-Twilio-VerStat`, plus what carriers without that
  header send).

The slip doesn't block the release — it reduces scope. That's
why the upstream PR is Week 1.

## 8. Protocol Versioning

All 0.4.0 additions are **additive** to v1:

- `BridgeOut::Start.verstat` is a new optional field (omitted
  in v1 servers' message structure when STIR/SHAKEN is off,
  present when on).
- CDR `verstat_attest` and `verstat_passed` are new optional
  fields. **CDR schema version decision**: stays at 1 if both
  fields are emitted only when populated (additive optional
  fields tolerated by parsers); bumps to 2 if we emit them as
  explicit `null` for unverified calls (some parsers reject
  unknown keys). Default plan: emit conditionally, stay at v1.

**Protocol stays at `version: "1"` for 0.4.0.** Servers built
against 0.1.0 / 0.2.0 / 0.3.0 keep working unchanged; they
ignore the new `verstat` field on `start`.

## 9. Decisions Before Sprint 1

Open questions that need resolution before W1 starts. Default
recommendation listed; revisit during the planning conversation.

1. ☐ **Trust anchor distribution.** Ship the iconectiv STI-PA
   root cert in `contrib/sti-pa-roots.pem` (operator copies into
   place) versus pulling it at runtime from a known URL? **Recommended**:
   ship it. The STI-PA root rotates rarely (years), and a runtime
   fetch is a startup-time dependency we don't currently have. Operator
   override via `[security.stir_shaken].trust_anchors` still possible.
2. ☐ **Verification cert cache eviction.** TTL-based
   (config-driven, default 1h) versus signature-count-driven?
   **Recommended**: TTL. Simpler invariant, matches HTTP cache
   semantics on the `info=` URL responses.
3. ☐ **Per-route `min_attestation` resolution semantics.** Strict
   override (route value wins even if more permissive than
   global) versus "max of global and route" (route can only
   tighten, never loosen)? **Recommended**: strict override,
   matching `[route.bridge.tls]` semantics from 0.3.0. Operators
   who want tighten-only can leave the route block out.
4. ☐ **Policy-rejection response code.** 403, 488, or 606?
   **Recommended**: 403 (default) with `Reason: Q.850;cause=21;text="STIR/SHAKEN attestation insufficient"`.
   606 ("Not Acceptable") is appropriate when the *call itself*
   is unacceptable; 488 is for media. 403 is the cleanest
   "policy rejected this call" code.
5. ☐ **CDR schema version bump.** Stay at 1 (emit fields only
   when populated) vs. bump to 2 (always-present optional
   fields)? **Recommended**: stay at 1. Less downstream churn.
6. ☐ **HEP chunk vendor ID.** Allocate a SiphonAI-specific
   vendor ID for the verstat chunk versus piggyback on an
   existing one? **Recommended**: register a new vendor ID with
   Homer upstream; carry it locally as `0x0000` (generic) for
   0.4.0 while the upstream registration is in flight.
7. ☐ **Sprint length.** 6 weeks (matches 0.3.0). One upstream PR
   makes a shorter sprint plausible; 4 weeks if scope tightens.
   **Recommended**: 6 weeks. The verification primitive is
   significant siphon-rs work; review latency was the dominant
   cost in 0.3.0.

## 10. Definition of Done — v0.4.0

A reasonable user can:

1. Enable STIR/SHAKEN verification on a US-originated Twilio
   inbound and see `verstat_attest: "A"` in the `BridgeOut::Start`
   message AND the CDR.
2. Reject inbound calls with attestation below `"B"` via
   `[security].min_attestation = "B"`, and observe
   `siphon_ai_invites_total{result="rejected_attestation"}`
   ticking.
3. See per-route attestation policy override a route that
   handles high-trust internal traffic.
4. Cross-reference our verification verdict against Twilio's
   `X-Twilio-VerStat` header and see them agree (the recipe
   demonstrates this).
5. Correlate STIR/SHAKEN verdicts against SIP dialogs in Homer
   via the new HEP chunk type.
6. CI gates every PR on build + clippy + fmt + cargo test +
   SIPp suite (now including STIR/SHAKEN pass and fail scenarios).
   Carries forward from 0.2.0 / 0.3.0.
7. Upgrade from 0.3.0 to 0.4.0 with no config changes and
   observe no behavioural difference — STIR/SHAKEN defaults to
   `enabled = false`.

## 11. Risks

Listed once, briefly:

- **Upstream PR review latency.** One PR (`sip-identity`) on the
  critical path. Smaller surface than 0.3.0's three coordinated
  PRs but the verification primitive itself is dense. Mitigation:
  §7 slip plan — pass-through-only as the fallback shape.
- **STI-PA cert chain availability.** The iconectiv-rooted chain
  is the de-facto US standard but isn't bundled in any OS trust
  store. Mitigation: §9 decision-1, ship the root in `contrib/`
  with documentation on how to update it.
- **PASSporT verification false positives.** A bug in our
  verifier that passes a malformed PASSporT is a serious fraud
  hole. Mitigation: lean heavily on test vectors from RFC 8225
  Appendix A; SIPp scenarios for every documented failure mode
  (bad signature, expired `iat`, mismatched `orig`/`dest`,
  cert-chain-fail, etc.).
- **Carrier interop variance.** STIR/SHAKEN in the wild has
  vendor quirks — different `info=` URL formats, JSON whitespace
  in the PASSporT (some sign with whitespace, some without),
  certs from non-iconectiv roots in some jurisdictions.
  Mitigation: log the raw Identity header verbatim alongside
  the parsed verdict so an operator can debug a one-off
  verification failure without losing the original wire bytes.
- **Operator confusion about "what attestation means".**
  Attest-A means "the carrier verified the originator owns
  the calling number" — NOT "this call is trusted." Bad actors
  can still get Attest-A calls placed if they control a valid
  number. Document this loudly in `docs/SECURITY_STIR_SHAKEN.md`
  (new in 0.4.0): attestation is a *signal*, not a verdict.

## 12. Out of scope (explicit non-goals for 0.4.0)

So nobody asks:

- Outbound attestation generation (we sign). See §6.
- Multi-jurisdiction trust roots (TBPN root etc.). See §6.
- STIR/SHAKEN over IPX / non-SIP transports. SiphonAI is SIP-only.
- ATIS-1000080 governance integration (STI-GA / STI-PA queries
  beyond the cert chain). The static trust anchor model is
  sufficient for v1 inbound verification.
- Real-time fraud scoring beyond attestation level. CDR + HEP
  data is the substrate for that — operators build the scoring,
  not us.
