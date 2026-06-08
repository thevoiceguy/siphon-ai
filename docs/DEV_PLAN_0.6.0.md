# SiphonAI 0.6.0 Development Plan — DRAFT

> **STATUS: DRAFT for review.** Theme is set; the §9 decisions are proposed,
> not locked — confirm/iterate before chunk 1 (the 0.4.x / 0.5.0 plans worked
> the same way). Effort + sequencing assume the §9 recommendations.

**Theme: outbound call origination — SiphonAI places calls, not just answers
them.**

Every release so far has been **inbound-only**: a PBX or trunk calls in,
SiphonAI answers (UAS) and bridges to a WS server. 0.6.0 inverts that:
SiphonAI **originates** a SIP call to a destination (a PSTN number via a
gateway, or a SIP endpoint), and on answer bridges the media to a WS server —
the same bridge, tap, and protocol it already uses. This is the **keystone**
that the deferred call-handling features have been waiting on:

- **Attended transfer** needs a consultation leg — an outbound call.
- **AMD** (human/voicemail detection) pays off most on an outbound dial.
- Callbacks, outbound notifications/surveys, click-to-dial all become
  possible.

CLAUDE.md §8 lists outbound as the post-v1 item that "changes auth, dialog
ownership, and SIP routing." This plan is that change.

## 0. Why this is buildable now (grounded, not assumed)

The 0.5.0 SRTP-rekey chunk taught us to verify upstream before planning. We
did, for outbound — and unlike re-key, **there are no upstream blockers**:

- **siphon-rs UAC originates calls today.** `IntegratedUAC::invite(target,
  sdp) -> CallHandle` sends the INVITE, builds the outbound dialog, handles
  provisional/final responses + ACK, and offers `cancel()` / `bye()`. 401/407
  challenges auto-retry via a `CredentialProvider` (per-realm digest). No gap.
- **forge generates SDP offers.** `SdpProfile::with_local_addr()` builds a
  local offer (our codecs + allocated RTP port); `negotiate_answer()` exists
  for the inbound side.
- **~80% of the call path is reusable** — `CallController`, `MediaTap`, the
  bridge, the call registry, CDR/webhooks/recording all work once a dialog +
  media session exist.

So 0.6.0 is **net-new siphon-ai glue**, not an upstream project:

1. **Answer → session binding** — the inverse of the inbound `accept_inbound`
   flow: take the peer's 200-OK answer SDP, bind the negotiated codec/RTP
   target back onto the forge session.
2. **The outbound call handler** — allocate session + generate offer → send
   INVITE via the UAC → await answer → bind → hand off to `CallController`
   (which is UAC-side now: it owns the dialog, sends BYE/CANCEL).
3. **The originate trigger** — an authenticated control to ask SiphonAI to
   place a call.

## 1. Cardinal rules, restated

- **Still no AI.** Outbound doesn't change this — the WS server is still the
  brain (now an *outbound* bot: a survey/notification/callback agent). The
  bridge and protocol are unchanged.
- **Still no shared per-call state (§4.4).** Each outbound call is one
  `CallController` with its own dialog and media. The originate trigger
  creates a call; it does not introduce a cross-call registry of mutable
  state.
- **New stakes: outbound spends money.** This is the first feature where
  SiphonAI *places billable calls* and exposes a control surface that
  triggers them. Toll-fraud / abuse / runaway loops are first-class concerns
  (§9, §11), not afterthoughts. Auth, a concurrency cap, and rate limiting
  ship **with** the originate API, not later.
- **Observability ships with the feature (§4.5).** Outbound CDRs, call-
  progress webhooks, and metrics are in-scope must-haves, not follow-ups.

## 2. Already shipped (context)

Inbound INVITE → route match → SDP answer → bridge → WS (0.1.0); operator
events, call-progress, hold/transfer/mute/DTMF; SRTP/mTLS/TLS; STIR/SHAKEN
(0.4.x); call recording (0.5.0). 0.6.0 adds the *originating* side and reuses
the bridge/tap/controller/registry/CDR machinery verbatim.

## 3. Recommended 0.6.0 scope (must-have)

### 3.1 Outbound media (offer / answer)

A `MediaSetup::originate_outbound` mirror of `accept_inbound`: allocate the
forge session (get the RTP port), generate the SDP **offer** via the
configured codecs, and — after the answer arrives — **bind the answer**
(negotiated codec + remote RTP target) back onto the session and attach the
`MediaTap`. The one genuinely new media helper is "apply remote answer to
session" (the inbound path only does offer→answer).

### 3.2 Outbound call handler (UAC)

An `OutboundOriginator` that, given a target + gateway + bridge config:
allocates media + offer (3.1) → `IntegratedUAC::invite()` → awaits the
dialog's provisional/final responses → on 200 OK binds the answer and hands a
session + tap to a `CallController` → on busy/decline/timeout reports a
failure outcome. The controller runs **UAC-side**: it owns the dialog and
issues BYE on teardown / CANCEL on a pre-answer abort. Reuses the existing
controller, registry, CDR, webhook, and recording paths.

### 3.3 Gateways (where outbound calls go)

A named **gateway** = the SIP trunk/provider SiphonAI dials *through*:
destination proxy/registrar, default From / caller-ID, codecs, and digest
credentials (fed to the UAC `CredentialProvider`). The originate request
names a gateway. See §9 decision 2 for whether a gateway is a new
`[[gateway]]` block, a reuse of `[[register]]`, or both.

### 3.4 Originate trigger (control API)

An authenticated HTTP endpoint to place a call — `POST` a JSON body
(`{to, gateway, ws_url|route, from?, headers?}`) → SiphonAI returns a
`call_id` immediately (202 Accepted) and the call proceeds asynchronously.
**Auth is mandatory on this endpoint** (it spends money — §9 decision 5), and
a **max-concurrent-outbound cap** + basic rate limit ship with it (§9
decision 6). The admin HTTP surface exists (`crates/telemetry/src/admin.rs`)
but has no auth and no originate endpoint today — both are net-new.

### 3.5 Outbound observability (must-have, same PR as the feature)

- **CDR**: `direction = "outbound"` (the field exists, reserved for this),
  plus the dial target and gateway. See §9 decision 4 on CDR versioning.
- **Webhooks**: call-progress lifecycle — `call_initiated` / `ringing` /
  `answered` / `failed` (with a cause: busy / declined / no_answer /
  unreachable / auth_failed) / `ended`. This is where AMD later plugs in.
- **Metric**: `siphon_ai_outbound_calls_total{result}` and a concurrency
  gauge.

## 4. Stretch (slip target)

- **Attended transfer (REFER + `Replaces`)** — now *unblocked* by outbound
  (the consultation leg is an outbound call; siphon-rs already has
  `create_refer_with_replaces`). A strong fast-follow, but it's a second
  feature on top of origination — see §9 decision 7 for in-0.6.0 vs 0.6.1.
- **Early media** — connect the WS bridge before answer so the bot can hear
  ringback / provide pre-answer audio (183 session-progress). Default is
  connect-on-answer (§9 decision 3); early media is additive.

## 5. Out of scope — the AI line (unchanged)

The outbound *bot* (what to say, when to hang up, voicemail-drop logic) is
the WS server's job, shipped as a reference example (e.g. an
`outbound-notify-bot-py`), never core. AMD (the audio classifier that tells
the bot "machine vs human") is its own theme (§6) — outbound makes it pay
off, but it carries a forge-media dependency.

## 6. Deferred / unlocked-but-later, and pinned targets

- **AMD** — human/voicemail detection. Outbound makes it valuable; still its
  own audio-analysis release (needs a `forge-amd` classifier). Pick up after
  0.6.0.
- **Conferencing / whisper / barge + call park** — the other big theme;
  still post-outbound. A dedicated release.
- **SRTP re-key** — tracked upstream as
  [forge-media#71](https://github.com/thevoiceguy/forge-media/issues/71).
- **Campaigns / scheduling** — bulk/scheduled outbound (dialer lists, pacing)
  is a layer *above* single-call origination; out of 0.6.0. The originate API
  is the primitive a campaign tool would call.

## 7. Chunk plan (proposed)

Each chunk is its own PR, landed on `main` before the next is based on it.
No upstream critical path — all siphon-ai glue over ready siphon-rs/forge
APIs.

| # | Focus | Deliverables |
|---|---|---|
| 1 | Outbound media | `MediaSetup::originate_outbound` (offer gen) + apply-remote-answer-to-session binding; unit tests over fixture SDP. No SIP yet. |
| 2 | Outbound call handler | `OutboundOriginator`: UAC `invite()` → dialog → answer-bind → `CallController` (UAC-side BYE/CANCEL). Failure outcomes (busy/decline/timeout). Behind no public trigger yet (drive from a test). |
| 3 | Gateways + safety | Gateway config (§9.2) + caller-ID + `CredentialProvider` wiring for digest trunks; `max_concurrent_outbound` cap + rate limit. |
| 4 | Originate API | Authenticated `POST` originate endpoint (§9.5) → returns `call_id` (202); request/response contract; admin auth. |
| 5 | Observability | CDR `direction=outbound` + target/gateway; call-progress webhooks; `siphon_ai_outbound_calls_total` + concurrency gauge; HEP if applicable. |
| 6 | Docs + tests + release | `docs/OUTBOUND.md`; a SIPp scenario where SIPp is the **UAS** answering SiphonAI's INVITE; CHANGELOG/README; tag + release. |

(Attended transfer, if §9.7 puts it in 0.6.0, is a chunk between 5 and 6.)

## 8. New surfaces & versioning

- **New control API** — the originate endpoint is a new public surface; spec
  it in a doc and treat it like the WS protocol (versioned, documented).
- **New config** — `[[gateway]]` (or `[[register]]` reuse) + an
  `[outbound]` block (concurrency cap, default caller-ID, API auth).
- **WS protocol** — **unchanged** (`version: "1"`). An outbound call uses the
  exact same `start` → audio → control messages; the WS server can't tell
  inbound from outbound except by the `start` metadata (which already carries
  direction-ish context — confirm in §9.8).
- **CDR** — `direction = "outbound"` (see §9.4 on whether a new enum value
  bumps the schema version).

## 9. Decisions before chunk 1 (proposed; confirm)

1. ☐ **Trigger mechanism.** Authenticated HTTP originate API vs config-driven
   scheduled calls vs both. **Recommended:** HTTP API — it's the primitive
   everything else (campaigns, click-to-dial) builds on; the admin surface
   already exists.
2. ☐ **Gateway config model.** New `[[gateway]]` block vs reuse `[[register]]`
   (dial through an endpoint SiphonAI is already registered to) vs both.
   **Recommended:** support **both** — `[[register]]` reuse for
   registrar/PBX trunks (credentials already there) and a `[[gateway]]` for
   static IP-auth trunks; the originate request names one.
3. ☐ **WS connect timing.** Connect the bridge on answer vs early media
   (pre-answer). **Recommended:** on answer (mirrors inbound); early media is
   §4 stretch.
4. ☐ **CDR versioning for `direction`.** A new `"outbound"` value on the
   existing `direction` field — keep schema v1 (the field was always there,
   documented as reserved) or bump to v2? **Recommended:** keep v1 but call
   it out in the release notes; strict consumers pinned to `"inbound"` should
   already tolerate the documented reserved value.
5. ☐ **Originate-API auth.** It places billable calls. Bearer-token auth on
   the originate endpoint (and ideally the whole admin surface) vs the
   current "bind to localhost / front with a reverse proxy" posture.
   **Recommended:** require a bearer token for originate, default-bind admin
   to localhost, and document the toll-fraud surface loudly.
6. ☐ **Abuse controls.** `max_concurrent_outbound` cap + a simple rate limit
   in 0.6.0? **Recommended:** yes — both are cheap and the lack of them is a
   real toll-fraud footgun.
7. ☐ **Attended transfer in 0.6.0?** It's unblocked by outbound. In-scope
   must-have, stretch, or a 0.6.1 fast-follow? **Recommended:** 0.6.1 fast-
   follow — keep 0.6.0 focused on the origination foundation; attended
   transfer is a distinct second feature.
8. ☐ **Does the WS `start` message distinguish inbound vs outbound?** Add a
   `direction` field to `start` (additive, protocol stays "1") so the bot
   knows which side it's on? **Recommended:** yes — additive and useful; an
   outbound bot behaves differently (it speaks first).
9. ☐ **Sprint length.** ~5–6 weeks (6 chunks). **Recommended:** 6.

## 10. Definition of Done — v0.6.0

1. A `POST` to the (authenticated) originate API places a call to a SIP/PSTN
   target through a configured gateway (digest auth handled), and on answer
   the media bridges to the WS server exactly like an inbound call — the bot
   talks to the callee.
2. Busy / decline / no-answer / unreachable / auth-failed each produce a
   clean failure outcome surfaced on a webhook + CDR; a pre-answer abort
   sends CANCEL; an answered call's teardown sends BYE.
3. `direction = "outbound"` on the CDR with target + gateway;
   `siphon_ai_outbound_calls_total{result}` + a concurrency gauge tick;
   call-progress webhooks fire.
4. `max_concurrent_outbound` is enforced; the originate endpoint rejects
   unauthenticated requests.
5. CI gates every PR (fmt + clippy + test + SIPp), with a SIPp scenario where
   SIPp answers SiphonAI's outbound INVITE.
6. Inbound is completely unaffected; the WS protocol stays `version: "1"`.

## 11. Risks

- **Toll fraud / abuse (the big new one).** An exposed or unauthenticated
  originate API can place unlimited billable calls. Mitigation: mandatory
  auth on the endpoint, `max_concurrent_outbound`, rate limiting, localhost-
  default binding, loud docs. This is §9.5/§9.6 and a DoD item, not a
  follow-up.
- **Call-progress semantics.** Mapping SIP responses (180/183/200/486/603/
  408/487) and timeouts to clean outcomes is fiddly and provider-dependent.
  Mitigation: an explicit outcome enum + a SIPp matrix covering the cases.
- **Media answer-binding correctness.** Applying the peer's answer to the
  forge session is the one genuinely new media step; getting codec/port/
  direction wrong = one-way or dead audio. Mitigation: fixture tests +
  the SIPp answer-path scenario asserting two-way RTP.
- **Dialog ownership inversion.** The controller is UAC-side now (it sends
  BYE/CANCEL, handles the callee's BYE). Mitigation: keep the controller's
  teardown paths symmetric; test caller-hangup and callee-hangup both.
- **Auth/credential handling for gateways.** Digest creds in config →
  redaction in logs, validation at load.

## 12. Out of scope (explicit non-goals for 0.6.0)

Campaign/bulk/scheduled dialing (the API is the primitive; the dialer is a
layer above), AMD (§6), conferencing/whisper/barge/park, SRTP re-key
(forge-media#71), video, WebRTC, the outbound *bot* logic itself (WS-server
example), and — pending §9.7 — attended transfer (recommended 0.6.1).
