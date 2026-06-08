# SiphonAI 0.5.0 Development Plan — DRAFT

> **STATUS: APPROVED — §9 decisions locked. Scope narrowed mid-sprint:
> the SRTP-rekey ride-along was DEFERRED (see §3.2 / §6) — forge-media has no
> coordinated re-key API. 0.5.0 ships recording only.**
> Executing chunk-by-chunk off `main` (land each before basing the next).

**Theme: call recording — compliance/QA capture of call audio.**

0.1.0 shipped the bridge. 0.2.0 operator primitives + call-progress. 0.3.0
made the wire defensible (SRTP / mTLS / hot-reload TLS). 0.4.x shipped
STIR/SHAKEN. 0.5.0 adds the one big remaining table-stakes contact-center
feature that needs **no AI, no conferencing, and no outbound origination**:
recording the call to storage for compliance and QA. (A timed SRTP re-key
was planned to ride along but was deferred once the upstream capability
turned out not to exist — see §3.2.)

### How we got to this theme

After mapping the wishlist, the other 0.5.0 candidates all hit a wall and
moved out:

- **AMD (human/voicemail detection)** — deferred to a later audio-analysis
  release. Pure audio analysis (a `forge-amd` sibling to `forge-vad`), so
  it's ready to pick up — but its real payoff is on *outbound* dials
  (post-v1), so it waits.
- **Call park** and **attended transfer** — both need a second leg
  (an outbound consultation call, or coordinated cross-call state that
  §4.4 forbids), so they belong with **outbound origination / conferencing**
  (§6), not here. The siphon-rs UAC *already* supports REFER+`Replaces`
  (`create_refer_with_replaces`); the blocker is the consultation leg, not
  the SIP primitive.

## 1. Cardinal rule, restated

Still no AI. Recording is a media-tap → storage sink; the brain stays in the
WS server. The hot-path rule (§4.3) is the binding constraint here: the
audio task must never block on recording I/O — frames go to a per-call
writer task over a bounded channel, and the writer does the file I/O. No
cross-call state (§4.4): each recording is owned by its `CallController`.
Observability ships with the feature (§4.5).

## 2. Already shipped (context)

Call-progress modes, operator events (silence/dead-air/RTP stats), call
handling (hold, blind transfer, mute, DTMF, hangup, clear, mark),
encryption (SRTP DTLS+SDES, mTLS, hot-reload TLS, WSS), and STIR/SHAKEN
(0.4.x). 0.5.0 does **not** re-propose any of these.

## 3. Recommended 0.5.0 scope (must-have)

### 3.1 Call recording

Capture the call's audio to storage. SiphonAI already taps both legs' PCM
(the media-glue tap that feeds the WS bridge); recording forks that stream
to a writer.

- **Capture**: tap the decoded PCM16 both directions. Default layout
  **dual-channel stereo** — caller on the left, bot/WS on the right — which
  is what QA and per-speaker transcription want (see §9 decision 2). Encode
  to WAV/PCM16 (no new codec dependency; matches the 8 k/16 k bridge path).
- **Off the hot path (§4.3)**: the audio task pushes frames onto a bounded
  per-call channel; a dedicated writer task buffers and writes. On sustained
  overflow the recording is flagged **degraded** (metric + the
  `RecordingStopped` reason) rather than blocking audio or silently lying
  about a gap (see §9 decision 6).
- **Control**: `[recording].mode` = `off` (default) / `always` (record every
  matched call) / `on_demand` (WS server drives it). On-demand adds
  `BridgeIn::StartRecording` / `StopRecording`, and — for PCI "stop while
  the caller reads a card number" — `PauseRecording` / `ResumeRecording`.
  `BridgeOut::RecordingStarted` / `RecordingStopped` / `RecordingFailed`
  carry a `recording_id` and (on the file sink) the path.
- **Config**: `[recording]` — mode, output dir + path template
  (`{date}/{call_id}.wav`), channel layout, per-route override
  `[route.recording]`. A pluggable sink (file first, like the CDR sink
  abstraction; object-storage is a later sink — §4).
- **CDR**: optional `recording_id` / `recording_path` (emitted only when
  populated → schema stays at version 1, per the 0.4.0 precedent).
- **Metric**: `siphon_ai_recordings_total{result="ok|degraded|failed"}`.
- **Lifecycle**: the writer is a per-call sub-task; on call end it flushes,
  finalizes the WAV header, closes, and the path lands on the CDR.

### 3.2 SRTP re-key on a timer (ride-along) — **DEFERRED**

Planned as the 0.3.0 carry-forward on the assumption that "the DTLS-SRTP
re-key crypto exists in `forge-rtp`; just expose the trigger." **That
assumption was wrong** (verified during chunk 4):

- forge-engine **explicitly blocks DTLS renegotiation** (post-handshake DTLS
  packets are dropped); `export_srtp_keys()` is one-shot.
- Only low-level `SrtpContext::set_local_key`/`set_remote_key` primitives
  exist — a **unilateral local swap breaks media** (the peer keeps the old
  keys → auth failures), so it's not a usable re-key.
- The SDES path needs a re-INVITE carrying fresh `a=crypto`, but siphon-ai's
  re-INVITE handler only does hold/resume direction changes, and siphon-ai is
  **UAS-only** — it can't originate a re-INVITE to push a rotation.

A real coordinated re-key therefore needs new **upstream forge-media** work
(DTLS-SRTP renegotiation / re-export) and/or UAS-initiated re-INVITE +
SDES regen in siphon-ai. That's a separate effort, not a "trigger." Moved to
§6 (deferred); 0.5.0 ships recording only.

## 4. Stretch (slip target)

- **Object-storage sink** (e.g. S3-compatible) behind the same sink trait as
  the file sink — for operators who don't want recordings on the call node's
  local disk. File sink is the must-have; this is additive.
- **Compressed format** (Opus) as an alternative to WAV, to cut storage. WAV
  is the must-have.

Both slip to 0.5.1 if Week 6 is tight.

## 5. Out of scope — the AI line (unchanged)

CLAUDE.md §4.1 keeps transcription, analytics (sentiment / compliance
scoring), translation, and semantic-event *generation* in the WS server,
shipped as reference examples. Recording produces the audio file those tools
consume; it does not analyze it. (A recording is, notably, the perfect input
to a WS-side transcription/QA bot — but that bot is an example, not core.)

## 6. Deferred — outbound, conferencing, AMD, and a pinned target

These keep coming up as prerequisites for the call-handling features that
left 0.5.0; pinning their homes:

- **Outbound origination** — the keystone that unlocks **attended transfer**
  and callbacks (and AMD's real payoff). Changes auth, dialog ownership, and
  SIP routing (CLAUDE.md §8). **Proposed: its own theme, 0.6.0 or 0.7.0.**
- **N-leg conferencing / whisper / barge** — and **call park** with it (true
  park needs the cross-call routing conferencing brings). The largest lift
  on the roadmap. **Proposed target: a dedicated theme once outbound lands.**
- **AMD** — a later audio-analysis release; the `forge-amd` primitive is
  ready to pick up when outbound makes it pay off.
- **SRTP re-key on a timer** (was §3.2) — needs a coordinated key rotation
  forge-media doesn't expose (DTLS renegotiation is blocked; SDES would need
  a UAS-initiated re-INVITE). **Next step:** an upstream forge-media issue/PR
  for a DTLS-SRTP re-key (renegotiate + re-export), then expose
  `[media.srtp].rekey_after_seconds` in siphon-ai. Target a later release.

Confirm the outbound-vs-conferencing ordering in §9 decision 8.

Also deferred (unchanged): video, WebRTC client, WS reconnect mid-call.

## 7. Chunk plan

No upstream critical path — recording is siphon-ai-only (it taps existing
forge PCM). Each chunk is its own PR, landed on `main` before the next is
based on it.

| # | Focus | Status | Deliverables |
|---|---|---|---|
| 1 | Recording capture core | ✅ #132 | Per-call writer task fed off the media tap over a bounded channel; WAV/PCM16 stereo sink to a local file. Hot-path-safe (no I/O on the audio task). `[recording].mode = "always"` path only. |
| 2 | Control + modes | ✅ #133 | `off`/`always`/`on_demand`; `Start`/`Stop`/`Pause`/`Resume` BridgeIn + `RecordingStarted`/`Stopped`/`Failed` BridgeOut; pause omits the span. |
| 3 | Config + CDR + metrics | ✅ #134 | `[recording]` + `[route.recording]` override; `recording_id`/`recording_path` on the CDR; `siphon_ai_recordings_total`; degraded-on-overflow signalling. |
| ~~4~~ | ~~SRTP re-key~~ | **deferred** | Moved to §6 — forge-media has no coordinated re-key API (§3.2). |
| 5 | Hardening + tests | next | SIPp scenario that records a call and asserts a valid non-empty WAV; docs (`docs/RECORDING.md`, CONFIG/PROTOCOL/DEPLOY already updated per chunk). |
| 6 | Release | — | Full smoke + SIPp suite green, CHANGELOG, version bump, tag, GitHub release. |

## 8. Protocol versioning

Additive — protocol stays `version: "1"`:

- New `BridgeIn`: `StartRecording` / `StopRecording` / `PauseRecording` /
  `ResumeRecording`.
- New `BridgeOut`: `RecordingStarted` / `RecordingStopped` /
  `RecordingFailed`.
- CDR gains optional `recording_id` / `recording_path` (emitted only when
  populated → schema stays at 1).

## 9. Decisions before Sprint 1 (proposed; confirm)

1. ☑ **Recording control model.** `off`/`always`/`on_demand` modes + WS
   control. **Decided:** ship all three — compliance wants `always`
   per-route, QA wants `on_demand`.
2. ☑ **Channel layout.** Dual-channel stereo (caller L / bot R) vs mono mix.
   **Decided:** stereo default (QA + per-speaker STT value), mono mix as
   a config option.
3. ☑ **Format.** WAV/PCM16 vs compressed. **Decided:** WAV first; Opus
   is §4 stretch.
4. ☑ **Sink.** Local file first vs object-storage now. **Decided:** file
   first behind a sink trait; object-storage is §4 stretch.
5. ☑ **Pause/resume in scope?** **Decided:** yes — PCI "pause while the
   caller reads a card number" is a core compliance need.
   ~~5b. SRTP re-key trigger.~~ **Deferred** (§3.2 / §6) — no forge-media
   re-key API.
6. ☑ **Overflow policy** when the writer can't keep up. **Decided:**
   flag the recording `degraded` (metric + `RecordingStopped` reason) and
   keep going — never block the audio task (§4.3), never silently drop.
7. ☑ **Retention / lifecycle.** Daemon-managed reaper vs operator-managed.
   **Decided:** operator-managed (storage/cron) — the daemon writes
   files + emits the path; no reaper in 0.5.0. Document it.
8. ☑ **Roadmap ordering** of outbound vs conferencing (§6). **Decided:**
   outbound 0.6.0, conferencing after.
9. ☑ **Consent/announcement.** **Decided:** out of core — the WS server
   plays any "this call is recorded" prompt; we document the operator's
   legal responsibility (two-party-consent jurisdictions).
10. ☑ **Sprint length.** 6 weeks.

## 10. Definition of Done — v0.5.0

1. With `[recording].mode = "always"`, a completed call leaves a valid,
   playable WAV (stereo: caller/bot separated) at the templated path, and
   the path is on the CDR.
2. A WS server can `StartRecording` / `StopRecording` mid-call in
   `on_demand` mode, and `PauseRecording` / `ResumeRecording` produce a
   recording with the paused span omitted.
3. Recording never blocks or gaps the live audio path; writer overflow is
   surfaced as `degraded`, not silent loss.
4. `siphon_ai_recordings_total` ticks by result; `[route.recording]`
   overrides the global.
5. CI gates every PR (fmt + clippy + test + SIPp), with a recording SIPp
   scenario asserting a non-empty valid WAV.
6. Upgrade from 0.4.1 is config-compatible — recording is `off` by default;
   no behaviour change.

(The SRTP-rekey DoD item was dropped — deferred per §3.2 / §6.)

## 11. Risks

- **Hot-path safety (the big one).** Recording I/O on the audio task would
  add jitter / drops to live calls. Mitigation: hard separation — bounded
  channel + dedicated writer task, file I/O never on the audio task; load
  test confirms no added jitter under recording.
- **Disk exhaustion.** Always-on recording fills disks. Mitigation: document
  sizing (PCM16 stereo ≈ 256 kbit/s ≈ ~115 MB/hour at 16 k); operator-
  managed retention; a `degraded`/`failed` result when writes error
  (ENOSPC) rather than a wedged call.
- **Legal/consent.** Recording has jurisdiction-specific consent law.
  Mitigation: documentation is explicit that consent + announcement are the
  operator's responsibility (§9 decision 9).
- **WAV correctness across pause/resume.** Finalizing the header and
  handling paused spans needs care. Mitigation: fixture tests that decode
  the output and assert duration/channels.

## 12. Out of scope (explicit non-goals for 0.5.0)

Outbound origination, conferencing/whisper/barge/park, attended transfer,
AMD, **SRTP re-key** (all §6), video, WebRTC client, WS reconnect mid-call,
recording *analysis* (transcription/QA — WS-server examples, §5), and a
daemon-side retention reaper (§9 decision 7).
