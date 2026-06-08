# SiphonAI 0.5.0 Development Plan — DRAFT

> **STATUS: DRAFT for review.** Scope and the §9 decisions are proposed,
> not locked. Confirm/iterate before Sprint 1 (the 0.4.0 plan worked the
> same way). Effort + sequencing assume the §9 recommendations.

**Theme: enterprise call-handling primitives — the SBC features a contact
center expects, minus AI and minus conferencing.**

0.1.0 shipped the bridge. 0.2.0 shipped operator primitives + call-progress
modes. 0.3.0 made the wire defensible (SRTP / mTLS / hot-reload TLS). 0.4.x
shipped STIR/SHAKEN call authentication. 0.5.0 fills the remaining
*non-AI, non-conferencing* gaps in call handling: detecting whether a leg
is a human or a machine (AMD), parking a call, completing the transfer
surface (attended transfer), and finishing the SRTP story (timed re-key).

These are deliberately the items from the original wishlist that (a) need
**no AI** (CLAUDE.md §4.1) and (b) **don't** require the N-leg conferencing
lift, which is pinned as its own future theme (§6).

## 1. Cardinal rule, restated

Still no AI code. AMD is **pure audio analysis** (cadence / energy / tone
patterns) — no STT, no LLM — so it lives in forge-media (sibling to
`forge-vad`), with SiphonAI surfacing its result as WS events. Everything
new is off by default or opt-in; observability ships with each feature
(§4.5).

## 2. Already shipped (context)

So the plan doesn't re-propose done work:

- **Call-progress modes** (instant_answer / ringing / session_progress) — 0.2.0.
- **Operator events** silence_detected / dead_air_detected, RTP stats
  (jitter / loss / rtt) — 0.2.0 / 0.3.x.
- **Call handling**: hold/unhold, blind transfer (REFER), mute/unmute,
  SendDtmf, hangup, clear, mark — shipped.
- **Encryption**: SRTP (DTLS-SRTP + SDES), mTLS bridge leg + SPKI pin,
  hot-reload SIP/TLS cert, WSS — 0.3.x.
- **STIR/SHAKEN** verification, policy gate, verstat surfaces, HEP chunk — 0.4.x.

## 3. Recommended 0.5.0 scope (must-have)

Three deliverables. AMD is the critical-path item (an upstream forge-media
capability); park and SRTP re-key are siphon-ai-only and small.

### 3.1 Answering-machine / voicemail detection (AMD)

Classify the **answered leg's audio** as human vs machine without any AI —
greeting cadence, energy envelope, speech-then-long-tail patterns, and beep
detection. Emits a WS event the server can act on (e.g. drop a voicemail,
or wait for the beep before playing a message).

- **Upstream**: a forge-media `forge-amd` crate (sibling to `forge-vad`),
  fed the decoded PCM it already has. Produces a classification +
  confidence + the time it settled, or `unknown` on timeout.
- **siphon-ai**: `media-glue` subscribes and emits a new `BridgeOut`
  event (see §9 decision 1 for shape). Config `[media.amd]` —
  `enabled` (default off), confidence threshold, analysis timeout,
  per-route override. CDR field + `siphon_ai_amd_total{result}` metric.
- **Scope note (be honest):** classic AMD pays off most on **outbound**
  dials (is this an answering machine?), which is post-v1. On inbound its
  value is robocall / voicemail-drop / IVR detection on the caller leg.
  Shipping the primitive now means it's ready the day outbound lands, and
  it's useful for inbound machine-detection today. If that framing isn't
  worth a release slot, say so — it's the one scope call worth challenging.

### 3.2 Call park (REFER-to-orbit)

A `BridgeIn::Park` that REFERs the caller to a configured parking-orbit URI
on the upstream PBX, which holds the call and provides retrieval (another
phone dials the orbit code). Reuses the existing blind-transfer REFER path
— small, and it keeps SiphonAI's "no cross-call state" invariant (§4.4): the
PBX owns the orbit and the pickup, not us.

- **siphon-ai**: `BridgeIn::Park { call_id, orbit }` (or a configured
  default orbit); on success the dialog is handed to the PBX exactly like a
  blind transfer; `BridgeOut::Parked` / `ParkFailed`. Config for a default
  orbit URI + an allowlist.
- **Why not in-bridge park:** true cross-endpoint pickup (park here, grab
  there) needs shared per-call state / a registry that §4.4 forbids and
  that conferencing (§6) would underpin. REFER-to-orbit sidesteps that by
  delegating to the PBX. See §9 decision 4.

### 3.3 SRTP re-key on a timer

The 0.3.0 SRTP carry-forward. The re-key crypto exists in `forge-rtp`
(DTLS-SRTP handshake re-key); 0.5.0 exposes the *trigger*:
`[media.srtp].rekey_after_seconds`. On the threshold, renegotiate keys
mid-call without dropping audio. Observable via a log line + a metric; no
protocol change.

## 4. Stretch (slip target)

### 4.1 Attended transfer (REFER with `Replaces`)

The 0.3.1 call-handling carry-forward. Completes the transfer surface:
`BridgeIn::Transfer` gains an attended mode that issues a REFER with a
`Replaces` header so the transferee joins an existing consultation dialog.

**Dependency:** needs a siphon-rs UAC capability (REFER/Replaces
construction) that isn't confirmed shipped — the same upstream-critical-path
shape STIR/SHAKEN had with `sip-identity`. If that capability is ready
early, promote to must-have; if it slips, this stays stretch and rolls to
0.5.1. Don't gate the AMD/park/re-key release on it.

## 5. Out of scope — the AI line (unchanged)

CLAUDE.md §4.1 keeps these on the WS-server side, shipped as reference
examples, not core features: real-time transcription, call analytics
(sentiment / escalation / compliance), live translation, AI provider
abstraction, and the *generation* of semantic events
(`intent_detected`, etc.). The protocol could grow an `IntentDetected`
carrier message for a server to ferry such events, but the producing brain
is never in the bridge. Example backlog (each its own PR, gates nothing):
`analytics-server-py`, `translator-bot-py`.

## 6. Deferred — conferencing, and a pinned target

**N-leg conferencing / whisper / barge is the single biggest unlock left**
(it underpins supervisor-whisper and N-party AI sessions) and the single
biggest lift: ~weeks of forge-media work to add N-leg per-call routing,
then siphon-ai work to expose conference primitives in the protocol.
CLAUDE.md §8 marks it post-v1 and the 0.3.0 plan called it "the largest
architectural lift on the roadmap."

**Proposed target: 0.6.0 as its own dedicated theme.** It is explicitly NOT
0.5.0. Pinning it here so the roadmap has a home for everything that depends
on it (supervisor whisper, N-party). Decide the target in §9.

Also deferred (post-v1, unchanged): outbound originated calls, recording,
video, WebRTC client, WS reconnect mid-call.

## 7. Sprint plan (6 weeks)

AMD's forge-media crate is the one critical-path dependency — open it Week 1
so review runs in parallel.

| Week | Focus | Deliverables |
|---|---|---|
| 1 | AMD upstream + scaffolding | Open the forge-media `forge-amd` PR (classifier + confidence + timeout). siphon-ai: `[media.amd]` config surface + the WS event type, behind `enabled = false`. No wire behaviour yet. |
| 2 | SRTP re-key | `[media.srtp].rekey_after_seconds` → timed DTLS-SRTP re-key, no audio drop; metric + log; SIPp/interop check. Independent of AMD upstream. |
| 3 | Wire AMD in | media-glue consumes forge-amd, emits the WS event; CDR field; metric; per-route override. First test against recorded voicemail-greeting vs live-speech fixtures. |
| 4 | Call park | `BridgeIn::Park` (REFER-to-orbit) reusing the transfer path; `Parked`/`ParkFailed`; config (default orbit + allowlist); SIPp scenario. |
| 5 | Attended transfer (stretch) OR hardening | If the siphon-rs UAC capability landed: REFER+Replaces attended transfer. Else: AMD tuning, more AMD fixtures, docs. |
| 6 | Hardening + release | Full smoke + SIPp suite green (incl. new scenarios), CHANGELOG, version bump, tag, GitHub release. |

## 8. Protocol versioning

All 0.5.0 additions are **additive** to v1 — protocol stays `version: "1"`:

- New `BridgeOut` AMD event (+ optional `IntentDetected` carrier is *not*
  in scope here).
- New `BridgeIn::Park` + `BridgeOut::Parked` / `ParkFailed`.
- Attended transfer is a new field/mode on the existing `Transfer` message.
- SRTP re-key is config-only (no wire change).

CDR gains optional AMD fields (emitted only when populated → schema stays
at version 1, per the 0.4.0 precedent).

## 9. Decisions before Sprint 1 (proposed; confirm)

1. ☐ **AMD event shape.** Single `amd_result { result: "human" |
   "voicemail" | "fax" | "unknown", confidence }` vs two events
   (`voicemail_detected` / `human_detected`). **Recommended:** single
   `amd_result` — covers `fax`/`unknown` cleanly and is one thing for a
   server to handle.
2. ☐ **AMD home.** New `forge-amd` crate vs extend `forge-vad`.
   **Recommended:** new sibling crate — keeps VAD lean and AMD's heuristics
   separable.
3. ☐ **AMD timeout behaviour.** Default analysis window (e.g. 3–4 s) and
   what a timeout emits. **Recommended:** ~3 s, emit `unknown` so the
   server always gets exactly one verdict.
4. ☐ **Park semantics.** REFER-to-orbit (delegates pickup to the PBX; fits
   §4.4) vs an in-bridge named-hold (no cross-endpoint pickup; brushes
   post-v1 WS-reconnect). **Recommended:** REFER-to-orbit.
5. ☐ **SRTP re-key trigger.** Time-based only (`rekey_after_seconds`) vs
   also packet/byte-count. **Recommended:** time-based only.
6. ☐ **Attended transfer: must-have or stretch?** Depends on the siphon-rs
   UAC REFER/Replaces capability. **Recommended:** stretch until the
   upstream is confirmed.
7. ☐ **Conferencing target release.** 0.6.0 dedicated theme vs later.
   **Recommended:** pin 0.6.0.
8. ☐ **Is AMD worth a 0.5.0 slot given inbound-only?** (§3.1 scope note.)
   **Recommended:** yes — ship the primitive; honest about the
   outbound payoff.
9. ☐ **Sprint length.** 6 weeks. **Recommended:** 6.

## 10. Definition of Done — v0.5.0

1. A call whose answered leg is an answering-machine greeting emits the AMD
   event as `voicemail` (and a live human as `human`); `[media.amd]` gates
   it; the result lands on the CDR and `siphon_ai_amd_total`.
2. A WS server can `BridgeIn::Park` a call to a configured orbit; the caller
   is REFERed and the PBX confirms; `Parked` / `ParkFailed` are surfaced.
3. `[media.srtp].rekey_after_seconds` triggers a mid-call SRTP re-key with
   no audio drop, visible in logs/metrics.
4. (stretch) Attended transfer (REFER+Replaces) completes against a PBX that
   supports `Replaces`.
5. CI gates every PR (fmt + clippy + test + SIPp), with new SIPp scenarios
   for park and (if landed) attended transfer.
6. Upgrade from 0.4.1 with no config changes and no behaviour difference —
   AMD/park/re-key are all opt-in.

## 11. Risks

- **AMD accuracy.** False voicemail/human calls are the core risk; mitigate
  with a confidence threshold, a recorded-fixture test corpus, and
  `unknown` rather than a forced guess. Tunable, off by default.
- **AMD upstream latency.** One critical-path forge-media PR (as
  `sip-identity` was for 0.4.0). Mitigation: open Week 1; SRTP re-key + park
  don't depend on it, so the release isn't blocked if AMD slips to 0.5.1.
- **Park architectural fit.** REFER-to-orbit keeps us clear of §4.4; if an
  operator expects in-bridge pickup, that's a conferencing-era feature —
  document the boundary.
- **Attended-transfer upstream dependency** (see §4.1).

## 12. Out of scope (explicit non-goals for 0.5.0)

Conferencing / whisper / barge (§6, pinned 0.6.0), outbound origination,
recording, video, WebRTC client, WS reconnect mid-call, and all AI features
(§5) — those are WS-server reference examples, never bridge code.
