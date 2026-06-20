# SiphonAI 0.6.0 Development Plan — DRAFT

> **STATUS: APPROVED — all §9 decisions locked.** Headlines: gateways =
> `[[register]]` reuse **+** `[[gateway]]` (§9.2); originate-API auth =
> reverse-proxy posture, no native token, so the abuse controls are the
> native guardrails (§9.5/§9.6); attended transfer = **0.6.1 fast-follow**
> (§9.7); WS protocol stays `version: "1"`. Ready to execute chunk-by-chunk
> off `main` (land each before basing the next).

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
  (§9, §11), not afterthoughts. The originate endpoint has **no native auth**
  (reverse-proxy posture, §9.5), so a **concurrency cap + rate limit +
  localhost-default bind** ship **with** it as the native guardrails, and
  "restrict access to this endpoint" is a documented operator responsibility.
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

A named **gateway** = the SIP trunk/provider SiphonAI dials *through*. The
originate request names one, and (decision §9.2, **locked**) a gateway is
**either**:

- an existing **`[[register]]`** entry — dial out through a PBX/provider
  SiphonAI is already registered to, reusing its server address and digest
  credentials; **or**
- a new **`[[gateway]]`** block — a static / IP-auth trunk: destination
  proxy, default From / caller-ID, codecs, and optional digest credentials
  (fed to the UAC `CredentialProvider`).

Both resolve to the same internal "where + how to dial" shape the outbound
handler consumes.

### 3.4 Originate trigger (control API)

An HTTP endpoint to place a call — `POST` a JSON body
(`{to, gateway, ws_url|route, from?, headers?}`) → SiphonAI returns a
`call_id` immediately (202 Accepted) and the call proceeds asynchronously.

**Auth posture (decision §9.5, locked): reverse-proxy, not a native token.**
The originate endpoint keeps the existing admin-surface model — no built-in
auth; the operator **must** bind admin to localhost (the default) and/or
front it with an authenticating reverse proxy. Because this endpoint spends
money, the **native guardrails are the abuse controls, not auth**: a
**`max_concurrent_outbound` cap** + a basic **rate limit** ship with it
(§9.6), the toll-fraud surface is documented loudly, and "you must restrict
access to this endpoint" is stated as operator responsibility. The admin HTTP
surface exists (`crates/telemetry/src/admin.rs`); the originate endpoint, the
cap, and the rate limit are net-new.

### 3.5 Outbound observability (must-have, same PR as the feature)

- **CDR**: `direction = "outbound"` (the field exists, reserved for this),
  plus the dial target and gateway. See §9 decision 4 on CDR versioning.
- **Webhooks**: call-progress lifecycle — `call_initiated` / `ringing` /
  `answered` / `failed` (with a cause: busy / declined / no_answer /
  unreachable / auth_failed) / `ended`. This is where AMD later plugs in.
- **Metric**: `siphon_ai_outbound_calls_total{result}` and a concurrency
  gauge.

## 4. Stretch (slip target)

- **Early media** — connect the WS bridge before answer so the bot can hear
  ringback / provide pre-answer audio (183 session-progress). Default is
  connect-on-answer (§9 decision 3); early media is additive.

(Attended transfer was a candidate here but is **locked to 0.6.1** — §6.)

## 5. Out of scope — the AI line (unchanged)

The outbound *bot* (what to say, when to hang up, voicemail-drop logic) is
the WS server's job, shipped as a reference example (e.g. an
`outbound-notify-bot-py`), never core. AMD (the audio classifier that tells
the bot "machine vs human") is its own theme (§6) — outbound makes it pay
off, but it carries a forge-media dependency.

## 6. Deferred / unlocked-but-later, and pinned targets

- **Attended transfer (REFER + `Replaces`)** — unblocked by outbound (the
  consultation leg is an outbound call; siphon-rs already has
  `create_refer_with_replaces`). **Locked to 0.6.1** as the fast-follow
  (§9.7): keep 0.6.0 focused on the origination foundation, then ship the
  transfer it unlocks as the next point release.
- **AMD** — human/voicemail detection. Outbound makes it valuable; still its
  own audio-analysis release (needs a `forge-amd` classifier). Pick up after
  0.6.0.
- **Conferencing / whisper / barge + call park** — the other big theme;
  still post-outbound. A dedicated release.
- **SRTP re-key** — the SDES upstream primitive landed
  ([forge-media#72](https://github.com/thevoiceguy/forge-media/pull/72)); the
  siphon-ai SDES re-key trigger is unblocked for a later release. DTLS-SRTP
  renegotiation is parked
  ([forge-media#73](https://github.com/thevoiceguy/forge-media/issues/73)).
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
| 3 | Gateways + safety | `[[register]]`-reuse **+** `[[gateway]]` config (§9.2) + caller-ID + `CredentialProvider` wiring for digest trunks; `max_concurrent_outbound` cap + rate limit (the native guardrails). |
| 4 | Originate API | `POST` originate endpoint → returns `call_id` (202); request/response contract. Reverse-proxy auth posture (§9.5) — localhost-default bind + loud toll-fraud docs; the cap/limit from chunk 3 are the native protection. |
| 5 | Observability | CDR `direction=outbound` + target/gateway; call-progress webhooks; `siphon_ai_outbound_calls_total` + concurrency gauge; HEP if applicable. |
| 6 | Docs + tests + release | `docs/OUTBOUND.md`; a SIPp scenario where SIPp is the **UAS** answering SiphonAI's INVITE; CHANGELOG/README; tag + release. |

(Attended transfer is **0.6.1**, not a 0.6.0 chunk — §6 / §9.7.)

## 8. New surfaces & versioning

- **New control API** — the originate endpoint is a new public surface; spec
  it in a doc and treat it like the WS protocol (versioned, documented).
- **New config** — `[[gateway]]` **+** `[[register]]` reuse (§9.2) + an
  `[outbound]` block (concurrency cap, rate limit, default caller-ID).
- **WS protocol** — **unchanged** (`version: "1"`). An outbound call uses the
  exact same `start` → audio → control messages, plus a new additive
  `direction` field on `start` (§9.8) so the bot knows which side it's on.
- **CDR** — `direction = "outbound"`; **schema stays at version 1** (§9.4) —
  the field was always there, reserved for this value.

## 9. Decisions before chunk 1 (proposed; confirm)

1. ☑ **Trigger mechanism.** **Decided: HTTP originate API** — the primitive
   everything else (campaigns, click-to-dial) builds on; the admin surface
   already exists. (Config-driven scheduled calls are out of scope, §12.)
2. ☑ **Gateway config model.** **Decided: both** — `[[register]]` reuse for
   registrar/PBX trunks (credentials already there) **and** a `[[gateway]]`
   block for static IP-auth trunks; the originate request names one.
3. ☑ **WS connect timing.** **Decided: connect on answer** (mirrors inbound);
   early media (pre-answer) is the §4 stretch.
4. ☑ **CDR versioning for `direction`.** **Decided: keep schema v1** — the
   `direction` field always existed and was documented as reserved for
   outbound; the new `"outbound"` value is called out in the release notes.
5. ☑ **Originate-API auth.** **Decided: reverse-proxy posture** — no native
   token on the endpoint; admin defaults to localhost-bind and the operator
   fronts it with an authenticating reverse proxy (or keeps it on a trusted
   network). The native guardrails are the §9.6 abuse controls + loud docs;
   restricting access to the endpoint is the operator's responsibility.
6. ☑ **Abuse controls.** **Decided: yes** — `max_concurrent_outbound` cap + a
   simple rate limit ship in 0.6.0, **doubly required given §9.5**: with no
   native auth on the endpoint, they are the only built-in defense against a
   runaway/abused originate path.
7. ☑ **Attended transfer in 0.6.0?** **Decided: 0.6.1 fast-follow** — keep
   0.6.0 focused on the origination foundation; ship attended transfer
   (REFER+Replaces) as the next point release on top of it.
8. ☑ **Does the WS `start` message distinguish inbound vs outbound?**
   **Decided: yes** — add a `direction` field to `start` (additive, protocol
   stays "1") so the bot knows which side it's on (an outbound bot speaks
   first).
9. ☑ **Sprint length.** **Decided: 6 weeks** (6 chunks).

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

- **Toll fraud / abuse (the big new one).** An exposed originate API can
  place unlimited billable calls — and per §9.5 the endpoint has **no native
  auth** (reverse-proxy posture). So the built-in defenses are
  `max_concurrent_outbound` + rate limiting + **localhost-default binding**,
  and access control is the operator's responsibility (front it with an
  authenticating proxy / trusted network). Mitigation is a DoD item, and the
  docs must state the "you must restrict this endpoint" responsibility
  loudly — the lack of native auth makes the cap/limit/bind non-negotiable.
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
layer above), **attended transfer (locked to 0.6.1, §6/§9.7)**, AMD (§6),
conferencing/whisper/barge/park, SRTP re-key (SDES primitive done,
forge-media#72; DTLS parked, forge-media#73), video, WebRTC,
and the outbound *bot* logic itself (WS-server example).
