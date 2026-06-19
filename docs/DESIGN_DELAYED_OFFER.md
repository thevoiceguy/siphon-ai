# Design note — SIP delayed offer (offerless INVITE)

> **Status: IMPLEMENTED — released v0.9.0 (chunks 1 inbound #190, 2
> outbound #191, 3 release); SDES SRTP follow-ups in v0.9.1 (outbound
> answer) + v0.9.2 (inbound offer).** v0.9.1 answers a peer's SDES offer
> on an outbound delayed call (the offerless INVITE can't *offer* SRTP) by
> running the inbound early-offer SDES path inside the gateway UAC's
> answer generator, governed by `[[gateway]].srtp`. v0.9.2 is the mirror:
> on an inbound delayed offer SiphonAI is the *offerer*, so `[media].srtp`
> `preferred`/`required` makes the 200 OK offer SDES and `apply_answer`
> installs the peer's ACK key. v0.9.3 adds **DTLS-SRTP on the
> outbound delayed answer** (gateway generator runs the inbound DTLS
> path, gained a per-process cert; the SRTP exchange dtls/sdes is carried
> on `OutboundAccepted`). v0.9.4 adds **DTLS on the inbound delayed-offer
> *offer*** — a new capability: `[media].srtp_offer = "dtls"` makes the
> 200 OK offer DTLS (`build_dtls_srtp_offer` patches the plaintext offer
> to SAVPF + our fingerprint + setup:actpass), and the ACK answer's
> fingerprint drives role negotiation + `enable_dtls`. **SRTP for delayed
> offer is now complete** (SDES + DTLS, both directions). v0.9.5
> delivered the **per-call CDR** for negotiations that fail before going
> active (CDR schema → v2) **and fixed a latent bug**: the daemon's
> packet pump was dropping the body-carrying ACK (never dispatching it to
> `on_ack`), so inbound delayed offer never actually bridged — the
> 200-OK tests masked it. All delayed-offer follow-ups are now done. WS
> protocol stayed `version: "1"`; CDR schema is v2.
>
> Outbound delayed offer (chunk 2): `POST /admin/v1/calls` with
> `delayed_offer: true` dials an offerless INVITE; the gateway UAC's
> `SdpAnswerGenerator` (a per-gateway answerer sharing a Call-ID-keyed
> registry with the originator) builds our answer from the peer's 2xx
> offer via `accept_inbound` and the ACK carries it. The session/tap come
> back to `place_delayed` through the registry's oneshot, then the shared
> outbound `run_call` bridges — so delayed-outbound legs get transfer/hold
> for free (offer_sdp = our answer text). One wrinkle resolved: the UAC's
> trait wants siphon-rs's `sip_sdp::SessionDescription`, distinct from
> forge-media's pinned sip-sdp, so core gained a direct `sip-sdp` dep and
> re-parses the answer text. SRTP on the delayed answer stays a follow-up
> (the offerless INVITE can't carry an SDES offer). Next: chunk 3 =
> config/docs/release (~v0.9.0).
>
> **Status: IN PROGRESS — chunk 1 (inbound) landed.** Inbound delayed
> offer is implemented (offerless INVITE → offer in 200 OK → answer from
> ACK → bridge), reusing the outbound `originate_offer` / `apply_answer`
> media path; the negotiation is gated in the accept path with a
> per-dialog held map + Timer-H watchdog (no new `CallState`). Two
> **scope notes for chunk 1**: (a) in-dialog **transfer/hold** and
> **SRTP-on-the-offer** for delayed-offer legs are deferred to a
> follow-up (the leg bridges audio without them); (b) the new failure
> modes surface as the `siphon_ai_delayed_offer_total{result}` **metric**
> + a `warn` log rather than a per-call **CDR** — a delayed-offer call
> that fails negotiation never became a call (no `CallStart`), matching
> how early-offer rejects behave today. The CDR `TerminationCause`
> vocabulary (§7) is still the plan for calls that *do* go active. Chunk
> 2 = outbound; chunk 3 = config/docs/release.
>
> **Status: DRAFT — gating decisions LOCKED (2026-06-17).** Same
> design-first pass we did for park / hold / reconnect / Opus, because
> delayed offer inverts the SDP offer/answer direction relative to every
> existing call path and touches the core accept flow, the media-setup
> ordering, and the WS-bridge start gate. No upstream change is needed
> (siphon-rs already supports both directions — see §3); this is
> siphon-ai wiring + state gating. The build follows this once locked;
> deviations get noted back here.

Adds **delayed offer** (a.k.a. *offerless INVITE* / *late offer*,
RFC 3264 §5–6): a call where the SDP offer is NOT in the INVITE. Today
SiphonAI **requires** the inbound INVITE to carry an SDP offer and
rejects an offerless one (`acceptor.rs::extract_offer_sdp` →
`OfferError::NoBody`). That forces interop partners — notably **Cisco
CUCM** — to insert a **Media Termination Point (MTP)** so CUCM generates
SDP in the initial INVITE. An MTP hairpins the media, consumes a CUCM
resource, and adds latency. Delayed-offer support removes the forced
MTP and lets media flow directly between the SIP endpoint and SiphonAI.

---

## 1. The core problem: the offer/answer direction inverts

Every call path SiphonAI has today is **early offer** — the side that
sends the INVITE provides the SDP offer:

| Path | Offer | Answer |
|---|---|---|
| Inbound (today) | peer, in INVITE | us, in 200 OK |
| Outbound (today) | us, in INVITE | peer, in 2xx |

**Delayed offer flips the back half** of each:

| Path | INVITE | Offer | Answer |
|---|---|---|---|
| **Inbound delayed** | peer, no SDP | **us, in 200 OK** | **peer, in ACK** |
| **Outbound delayed** | us, no SDP | **peer, in 2xx** | **us, in ACK** |

The CUCM case is **inbound delayed**:

```
CUCM                         SiphonAI
 | INVITE (no SDP)              |
 |---------------------------->|
 | 100 Trying                  |
 |<----------------------------|
 | 200 OK (SDP offer)          |   ← we allocate RTP + offer our codecs
 |<----------------------------|
 | ACK (SDP answer)            |   ← peer picks a codec + its RTP addr
 |---------------------------->|
 |<========== RTP ============>|
```

The consequence that ripples into the WS layer: **in early offer we know
the negotiated codec when we answer; in delayed offer we don't know it
until the ACK.** So the WS `start` message (which carries
`audio.sample_rate`) and the entire bridge/tap **must be deferred until
the ACK answer is parsed**. This is exactly "don't mark the call active
before negotiation completes."

---

## 2. What exists today (the building blocks already line up)

The media operations delayed offer needs already exist in `media-glue`,
just wired to the *other* signaling moments:

- **`MediaSetup::accept_inbound(InboundCall { offer_sdp, … })`** — parse a
  *received offer*, allocate the forge session, build *our answer*.
  (Used by inbound early offer.)
- **`generate_offer(caps, srtp)` + `MediaSetup::apply_answer(OutboundOffer,
  answer_sdp, tap)`** — build *our offer*, allocate the session, then
  apply a *received answer*. (Used by outbound origination.)

Delayed offer reuses these with the signaling inverted:

| New path | Make offer / parse offer | Apply answer |
|---|---|---|
| **Inbound delayed** | `generate_offer` + allocate (like outbound) | on ACK: `apply_answer(...)` with the ACK body |
| **Outbound delayed** | on 2xx: parse peer offer (like inbound) | answer in ACK via `accept_inbound`-style build |

So the heavy media lifting is **already written and tested**. The new
work is: detect offerless, drive the inverted signaling, gate the WS
bridge until the answer lands, and add the error/CDR surface.

---

## 3. Upstream capability — supported today, NO siphon-rs PR

Verified against the pinned siphon-rs checkout (`700f3dc`):

**Inbound (UAS).** `sip_uas::IntegratedUAS::accept_invite(request,
sdp_body: Option<&str>)` (and the session-timer variant we already call)
embeds whatever `sdp_body` we pass into the 200 OK **independent of
whether the INVITE had a body** — so we can put our *offer* there. The
`UasRequestHandler::on_ack(&self, request, dialog)` callback surfaces the
ACK **with its body** (`request.body()`), which is where the peer's
*answer* arrives. siphon-rs keeps no offer/answer state — that's ours to
track, which we'd do regardless.

**Outbound (UAC).** `sip_uac::IntegratedUAC::invite(target, sdp_body:
Option<&str>)` accepts `None` for a late offer (the doc comment says so
explicitly), and the builder exposes an `SdpAnswerGenerator` trait
"invoked when receiving a 200 OK with SDP offer after sending an INVITE
without SDP (late offer flow per RFC 3264)" — its generated answer goes
into the ACK. So the outbound half is a native, supported flow too.

**Verdict:** both directions are wiring-only on our side. If anything,
SiphonAI's offer/answer *negotiation* (codec pick, our SDP shapes) stays
in `media-glue` where it already lives; siphon-rs just carries bytes.

---

## 4. Negotiation modeling — keep `CallState`, gate in the accept path

**(Decision §7.1, LOCKED.)** The existing `core::CallState`
(`Initializing → Connecting → Active → Terminating → Done`) is the
**CallController sub-task lifecycle** — it describes WS-bridge + media-tap
tasks, not SIP negotiation. We do **not** expand it with SIP-negotiation
states. Instead the offer→200→ACK-answer exchange is a **phase in the
accept path, before the `CallController` is created.** The controller
(and therefore `Active`) only comes into existence once media is
negotiated — so "no Active before negotiation" falls out for free, with
no new enum states and no half-initialized controller.

The phase names from the requirement become **log span events + CDR
phase labels**, not types:

| Phase (log/CDR label) | What's happening | Where |
|---|---|---|
| `negotiating` | offerless INVITE detected; RTP allocated; offer built; 200 OK sent | accept path |
| `awaiting_ack_answer` | 2xx sent, waiting for the ACK (Timer H) | accept path |
| `connecting_bridge` | ACK answer parsed; codec known; WS bridge + tap starting | `CallController` spawn |
| `active` | bridge + tap running | `CallState::Active` |

Early-offer calls skip straight from accept to `connecting_bridge` as
they do today — **zero behaviour change** for the existing path.

---

## 5. Inbound delayed-offer flow (the CUCM case)

In `acceptor.rs`, when `extract_offer_sdp` returns `OfferError::NoBody`
(and the route/config permits delayed offer — see §7.4):

1. **Allocate + offer.** Build `LocalCapabilities` from the matched
   route's codecs and call the offer side of `MediaSetup` (allocate the
   forge session, `generate_offer`). Hold the resulting `OutboundOffer`
   (session + offered caps) keyed by dialog.
2. **200 OK with our offer.** `accept_invite_with_session_timer(request,
   Some(our_offer_sdp), …)` — same call we already use, now carrying an
   offer instead of an answer.
3. **Park the half-negotiated call** awaiting the ACK. The held state
   (offer caps + session + matched route/bridge config) lives in a
   small per-dialog map in the acceptor, NOT in a `CallController` (which
   doesn't exist yet). Timer H (the server transaction's ACK wait, ~32s
   — Decision §7.3) bounds it.
4. **On ACK** (`on_ack` callback): pull the held state for the dialog,
   read `request.body()` as the answer, `apply_answer(offer, answer, tap)`
   → negotiated codec + peer RTP addr. Then **create the `CallController`**
   and start the WS bridge/tap exactly as the early-offer path does
   post-negotiation. The WS `start.audio.sample_rate` reflects the
   codec we just learned.
5. **No ACK / bad ACK** → tear down with the right CDR reason (§6), release
   the forge session.

This keeps per-call state owned (CLAUDE.md §4.4): the held entry is a
short-lived acceptor-side map keyed by dialog id, emptied on ACK or
timeout; it is not cross-call shared state.

---

## 6. Outbound delayed-offer flow

(In scope per Decision §7.2.) On the origination path (`outbound.rs`):

1. **Offerless INVITE.** `uac.invite(target, None)` (or
   `invite_with_headers`), configured with an `SdpAnswerGenerator` that
   defers to `media-glue` (`accept_inbound`-style: parse the peer's offer
   from the 2xx, allocate the session, build our answer).
2. **2xx carries the peer's offer** → the generator builds our answer →
   siphon-rs puts it in the ACK.
3. **Start the bridge** once the answer is committed (codec known), same
   deferred-start gate as inbound.

Both directions converge on the same rule: **the `CallController` /
WS bridge starts only after the answer is in hand.**

---

## 7. Error handling & CDR

New failure modes get explicit detection, a log at `warn`, a metric
label, and a **CDR termination reason**. Add these variants to
`cdr::schema::TerminationCause` (snake_case on the wire):

| Reason | Trigger |
|---|---|
| `ack_timeout` | Timer H fired; no ACK arrived |
| `missing_sdp_answer` | ACK arrived with no body |
| `invalid_sdp_answer` | ACK body present but unparseable SDP |
| `no_compatible_codec` | answer selected nothing we offered |
| `invalid_remote_media` | answer's RTP address/port missing or unusable, or audio stream rejected (port 0) |

Adding enum variants is additive on the wire (new strings), but per
CLAUDE.md §7.7 anything beyond an additive optional field bumps the CDR
`version` — **confirm at build time** whether the new reason strings
warrant a version bump (likely yes, since parsers switch on the set).

A new counter, e.g. `siphon_ai_delayed_offer_total{result="…"}` with
bounded labels (`answered` / `ack_timeout` / `missing_sdp_answer` /
`invalid_sdp_answer` / `no_compatible_codec` / `invalid_remote_media`),
covers the §4.5 observability bar.

---

## 8. WS protocol impact — none (stays v1)

No new WS message and no shape change. Delayed offer only **defers** the
existing `start` until the codec is known; `start.audio.sample_rate`
already carries 8000/16000 from the negotiated codec, and that's
populated from the ACK answer instead of the INVITE offer. The protocol
stays `version: "1"`. (Worth a one-line note in `PROTOCOL.md` that
`start` may be delayed by one SIP round-trip on offerless INVITEs, but no
field changes.)

---

## 9. Decisions

**LOCKED (2026-06-17, via review):**

1. **State machine — keep `CallState`, gate in the accept path.** No new
   enum states; the negotiation phase lives before `CallController`
   creation; phase names are log/CDR labels. §4.
2. **Scope — inbound AND outbound** delayed offer. Both are
   siphon-rs-supported today; build both. §5, §6.
3. **ACK-answer timeout — reuse SIP Timer H (~32 s).** No new config knob;
   the server transaction's existing ACK wait governs it, firing
   `ack_timeout` on expiry. §5.

**To confirm (during the build):**

4. **Opt-in switch.** Recommend delayed offer is **accepted by default**
   when an offerless INVITE arrives (the whole point is interop), but
   provide a config gate (e.g. `[sip].allow_delayed_offer`, default
   `true`) so an operator can force-reject offerless INVITEs (back to the
   current 4xx) if they want strict early-offer. Confirm default +
   field name.
5. **CDR version bump** for the new termination reasons (§7) — likely
   yes; confirm at build time.
6. **4xx for the reject path.** When delayed offer is disabled (or fails
   pre-flight), keep returning the current code for an offerless INVITE.
   Today `OfferError::NoBody` maps to 400; RFC-wise an offerless INVITE
   is legal, so a *disabled* reject is better as 488/606 — confirm.

---

## 10. Implementation chunks

1. **Inbound delayed offer (core).** Detect offerless in `acceptor.rs`;
   allocate + `generate_offer`; 200 OK with offer; held-dialog map;
   `on_ack` → `apply_answer` → spawn `CallController`; Timer-H teardown.
   New `TerminationCause` variants + metric. Unit tests + a SIPp
   `delayed_offer_caller.xml` (UAC sends offerless INVITE, expects 200
   with offer, sends ACK with answer).
2. **Outbound delayed offer.** `uac.invite(target, None)` +
   `SdpAnswerGenerator` bridging to `media-glue`; deferred bridge start;
   SIPp roles-inverted scenario.
3. **Config gate + docs + release.** `[sip].allow_delayed_offer` (§7.4);
   `PROTOCOL.md` note (§8); `CONFIG.md`; `DEPLOY.md` metric; CDR version
   decision; CHANGELOG; SIPp `run-all.sh` phases; release.

Targets ~**v0.9.0** (a notable new capability; additive, protocol v1).

---

## 11. Testing

- **Unit (`media-glue`/`core`):** offerless detection; inbound
  offer→answer round-trip reusing `apply_answer`; each error path maps to
  its CDR reason; early-offer path unchanged (regression).
- **SIPp:** an inbound `delayed_offer_caller.xml` (offerless INVITE →
  assert 200 carries our `m=audio` + codecs → ACK with answer → RTP/BYE),
  and an outbound roles-inverted scenario. Both added to `run-all.sh` as
  always-on phases.
- **Interop note:** the acceptance bar is a real CUCM trunk/phone with
  **MTP Required disabled** sending an offerless INVITE and media flowing
  without an MTP. Lab-validated separately (`test-harness/interop`); the
  SIPp phases cover the signaling contract in CI.
