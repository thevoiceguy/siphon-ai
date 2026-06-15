# Design note — bot-initiated call hold / resume

> **Status: APPROVED — decisions LOCKED (§10).** Pins the WS surface, the
> SIP re-INVITE mechanics, the media handling, and the controller
> interaction before implementation — the same design-first pass we did
> for park (`docs/DESIGN_0.7.0_PARK.md`), because this is invasive SIP
> work (we become the re-INVITE *offerer* for the first time). The build
> follows this; deviations get noted back here.

Lets the **WS server (the bot) put its own caller on hold and resume
them** — "user asks to hold → bot holds the call." A `hold` re-INVITEs
the caller so their media goes on hold (they hear MOH, stop sending), the
bot stays connected on the same WS session, and a later `resume`
re-INVITEs back to two-way audio. This is the missing call-control
primitive: the bot can already transfer / hangup / park / record / mute /
DTMF / conference, but **cannot initiate hold** (today's `hold`/`resume`
are *inbound events* reporting that the **peer** held *us*, §3.3).

Companion deliverable (docs only, §9): clarify that a bot retrieves its
**parked** calls via the admin API from its backend — a parked call has no
WS, so retrieve can't be a WS message; hold is the primitive for "pause &
resume myself."

---

## 1. Hold vs. the things it is *not*

| | Dialog | Caller hears | Caller→bot audio | WS session | Resumes how |
|---|---|---|---|---|---|
| **`mute`** (exists) | `sendrecv` | silence | still flows | stays | `unmute` |
| **park** (exists) | `sendrecv` (media-only) | MOH | dropped | **detached** | operator/admin retrieve |
| **hold** (this) | re-INVITE → `sendonly`/`recvonly` | MOH | dropped | **stays** | bot sends `resume` |

Hold is "park the *media* (MOH) **+** signal the caller via re-INVITE **+**
keep the WS so the bot resumes itself." It reuses park's `MohSource` + tap
MOH mode for the media side and adds the re-INVITE the bot drives.

**Locked-in decision (per kickoff): true SIP re-INVITE hold**, not a
media-only hold — the far end is signalled `sendonly`/`recvonly` per RFC
3264 §6.1, so a PBX/carrier in the path knows the call is held (can stop
its own media, bill differently) rather than just hearing music over a
still-`sendrecv` stream.

---

## 2. WS protocol additions (version stays `"1"` — additive)

- `BridgeIn::Hold { call_id }` — put **this** call's caller on hold
  (self-scoped, §9.2 of the conferencing model). No-op if already held.
- `BridgeIn::Resume { call_id }` — return to two-way audio. No-op if not
  held.
- `BridgeOut::Held { call_id, seq }` — confirmation the hold took effect
  (the re-INVITE got a 2xx and the caller is on MOH). Sent **after** the
  round-trip, so the bot knows hold is real before it relies on it.
- `BridgeOut::Resumed { call_id, seq }` — confirmation two-way audio is
  back.
- `ErrorCode::HoldFailed` — `error { code: "hold_failed" }` when the
  re-INVITE is rejected, times out, or glare can't be resolved. The call
  **stays in its prior state** (still `sendrecv` on a failed hold; still
  held on a failed resume) — a failed hold never drops the call.

**Naming:** the request verbs are `hold` / `resume` (`BridgeIn`); the
confirmations are the past-tense `held` / `resumed` (`BridgeOut`),
deliberately distinct from the existing **peer-initiated** `hold` /
`resume` events (§3.3) so a server never confuses "I was put on hold by
the far end" with "my hold request succeeded." PROTOCOL.md §3/§4 updated
in the same PR, including a table contrasting the two.

*Decision to confirm (§10.1):* keep the success acks (`held`/`resumed`),
or fire-and-forget with only `hold_failed` on error (simpler, mirrors
`mute`). **Recommend the acks** — hold is an async round-trip, unlike the
instant `mute`.

---

## 3. SIP mechanics — we become the re-INVITE offerer

Today SiphonAI only *answers* re-INVITEs (`on_reinvite` → `BridgeOut::Hold`
when the **peer** holds us; `crates/sip-glue/src/handler.rs`,
`crates/core/tests/hold_resume.rs`). Bot-hold makes us the **offerer** for
the first time. Mirrors transfer's controller→UAC drive
(`call.rs::run_transfer` → `ctx.uac.send_refer(&mut dialog, …)`):

1. **Build the hold offer SDP.** The current negotiated audio (same
   port/codec the call answered with) with the audio direction set to
   `a=sendonly` (we keep sending — the MOH — and stop expecting caller
   media). media-glue gains `generate_hold_offer(local, MediaDirection)`
   alongside `generate_offer` (sdp.rs already has `MediaDirection` +
   `with_direction`).
2. **Send the re-INVITE.** `uac.send_reinvite(&mut dialog, Some(&hold_sdp))`
   (siphon-rs `IntegratedUAC::send_reinvite`, exists). `send_in_dialog_invite`
   **auto-ACKs the 2xx**, so we don't hand-roll the ACK. Returns the
   `Response`.
3. **Classify the response:**
   - **2xx** → hold is live: drive the media side (§4), set `held`, emit
     `held`. (The 200 OK's answer should be `recvonly`/`inactive`; we
     don't strictly need to parse it — we already stopped expecting caller
     media — but we validate it's a 2xx.)
   - **491 Request Pending (glare)** — the peer re-INVITEd at the same
     time. RFC 3261 §14.1: wait a bounded random backoff (UAC that was the
     offerer: 2.1–4.0 s) and retry once; if it 491s again, give up →
     `hold_failed`. (A first cut may retry once then fail; documented.)
   - **other non-2xx** (488 etc.) → the peer refused hold → `hold_failed`,
     stay `sendrecv`.
4. **Resume** is the same with `a=sendrecv` (the unhold offer), restoring
   two-way media on 2xx → `resumed`.

**Dialog access.** The controller already reaches the UAC + dialog for
REFER (`TransferContext { uac, source: DialogSource }`). Hold needs the
same. Rather than a parallel context, generalise: rename/extend the
in-dialog control carrier so it serves **both** REFER and re-INVITE (e.g.
a `DialogControl { uac, source }` the controller holds; transfer and hold
both borrow it). Inbound legs use the managed dialog manager; outbound
legs the direct dialog — exactly as transfer already distinguishes
(`DialogSource::Managed` vs `Direct`, and the TCP/TLS `via_flow` variants).
So **hold inherits transfer's connection-reuse fix for free** — the
re-INVITE goes out the same flow the inbound INVITE arrived on (the 0.6.2
`*_via_flow` work), which matters on TCP/TLS trunks.

---

## 4. Media handling during hold

Reuse park's media path wholesale:

- **On hold (after the 2xx):** `TapCommand::Park { moh }` — the tap stops
  forwarding caller→WS audio, stops the direct playout pair, and plays
  `MohSource` on the 20 ms tick into the caller leg. The caller hears hold
  music; the bot stops "hearing" the caller (no barge-in during hold).
- **On resume (after the 2xx):** `TapCommand::Unpark { … }` restoring the
  direct caller↔WS pair.

**One tap tweak vs. park:** under park the WS is gone, so playout simply
stops. Under hold the **WS stays open** and the bot may keep streaming
audio — the tap's MOH mode must **drain-and-drop** `playout_audio_rx`
during hold so a chatty bot doesn't back-pressure the channel. (The bot
*should* pause on `held`, but we don't depend on it.) Small addition to
the existing Park arm.

**MOH source.** Resolve the same way park does (`MohSource::new(moh_file,
rate)` → looping `FileSource` or comfort-noise fallback). Config decision
in §10.2: reuse `[park].moh_file`, or add `[hold].moh_file` / a shared
`[media].moh_file`. **Recommend a shared `[media].moh_file`** that both
park and hold read, so hold doesn't require `[park].enabled`. Hold itself
needs **no enable flag** — it's a basic call-control primitive like
hangup/transfer, always available to the bot.

---

## 5. Controller lifecycle

Add `held: bool` (false initially). The command path (where WS `BridgeIn`
messages are handled, alongside the existing transfer/park/conference
arms):

- **`Hold`** (ignore if `held`, or if the call is in a conference / a
  transfer is in flight — §6): build the hold SDP, `send_reinvite`
  (awaited inline in the command arm, same as REFER — the RTT is tens of
  ms), classify (§3). On success: `tap_cmd_tx.send(Park{moh})`,
  `held = true`, emit `held`, bump metric, account hold start. On failure:
  emit `error{hold_failed}`, leave state unchanged.
- **`Resume`** (ignore if `!held`): re-INVITE `sendrecv`, on 2xx
  `tap_cmd_tx.send(Unpark{…})`, `held = false`, emit `resumed`, account
  hold duration.

The bridge-end / tap-end / shutdown arms are unchanged: a held call tears
down like any other (the held media state doesn't gate teardown). No new
durable-task rework like park needed — the WS bridge stays attached
throughout, so this is materially simpler than park's lifecycle surgery.

---

## 6. Interactions & edge cases

- **Peer already holds us, then bot holds** → we're already `recvonly`
  toward the peer; our hold re-INVITE makes it `inactive`. Allowed; the
  resume restores to whatever the peer's current direction implies.
  (First cut may simply reject a bot-hold while a peer-hold is active and
  log it — §10.3.)
- **Conference** → reject `hold` while the call is in a room
  (`hold_failed`); the room owns the media path. (Leave the room first.)
- **Transfer in flight** → reject `hold` until the REFER resolves.
- **Park vs hold** → distinct: park detaches the WS (operator retrieve),
  hold keeps it (bot resume). A `park` on a held call is allowed and
  supersedes (park's detach wins); a held call's MOH simply continues.
- **Double hold / resume-when-not-held** → no-ops (idempotent), no error.
- **Glare** → §3 step 3 (491 backoff+retry-once).
- **Teardown while held** (caller BYE, WS close, daemon shutdown) →
  normal teardown; nothing special.
- **Recording while held** → the recording keeps writing and captures the
  MOH (consistent with park: "what the caller heard").
- **No hold timeout.** Unlike park, the WS stays open, so an abandoned
  hold (bot crash) closes the WS → the call tears down on
  `ws_disconnect`. No timer needed.

---

## 7. Observability

- **Metric:** `siphon_ai_holds_total{result=ok|failed}` (hold attempts;
  `failed` = re-INVITE rejected/glare). Optionally a
  `siphon_ai_held_calls_active` gauge.
- **CDR (additive, schema stays v1):** `hold { count, total_ms }` —
  mirror park's accounting exactly (`crates/cdr/src/schema.rs` `ParkInfo`
  → a parallel `HoldInfo`). Omitted when the call was never held.
- **Logs:** `call held` / `call resumed` / `hold re-INVITE rejected` at
  `info`/`warn` with `call_id` in the span.
- **No webhook** (hold is a transient in-call bot action, not a lifecycle
  event an out-of-band consumer needs — unlike park's
  `call_parked`/`call_retrieved`).

---

## 8. SIP regression (chunk-N / release)

A SIPp phase mirroring `reinvite_hold_resume.xml` but **inverted**: the
WS server (echo) sends `hold` then `resume` (a new `--auto-hold` harness
knob), and the SIPp caller asserts it **receives** a re-INVITE with
`a=sendonly` (hold) then one with `a=sendrecv` (resume), answering each.
Cross-check `siphon_ai_holds_total{result="ok"}`. Audio content isn't
asserted (signalling test); the MOH/media reuse is covered by park's
unit tests.

---

## 9. Companion docs — bot retrieves its *parked* calls (no code)

A parked call's WS is **closed** (`stop{park}` + close — that's the point:
the bot is freed). So there is no WS channel for a `retrieve` message, and
none is added. The bot drives park-retrieve from its **backend**:

1. Bot (over its live WS, or admin) parks the call; it holds the
   `call_id`.
2. When ready, the bot's server calls
   `POST /admin/v1/calls/:id/retrieve { ws_url }` with its own `ws_url`.
3. SiphonAI opens a **fresh** WS to the bot with `start { retrieved: true }`;
   the bot continues.

`docs/PARK.md` + `docs/OUTBOUND.md`-style guidance gets a "managing your
own parked calls" section making this explicit, and contrasting it with
**hold** ("for pause-and-resume on the live session, use hold, not park").

---

## 10. Decisions — LOCKED (2026-06-15)

1. **Success acks** — **YES.** SiphonAI sends `held` / `resumed` after the
   re-INVITE 2xx; `hold_failed` on error. §2.
2. **MOH config** — **shared `[media].moh_file`**, read by both park and
   hold; hold needs no `[park].enabled`. Comfort-noise fallback when unset
   / rate-mismatch (park's `MohSource` rules). §4. *(Park's `[park].moh_file`
   stays for back-compat — see §11.)*
3. **Peer-hold + bot-hold composition** — **reject + log** a bot-hold while
   the peer already holds us, first cut (`hold_failed`). `inactive`
   stacking is a later refinement. §6.
4. **Dialog plumbing** — **generalise** transfer's `uac` + `source` carrier
   into one `DialogControl` the re-INVITE path also uses. §3.
5. **Version** — **0.7.2** (additive bot primitive, no enable flag).
6. Everything else follows the park / transfer precedents.

## 11. Implementation chunks

Mirrors the park / outbound-SRTP cadence — plan PR, then chunked impl PRs,
then SIPp + release.

- **Plan** (this note). 
- **Chunk 1 — WS protocol surface.** `BridgeIn::Hold` / `Resume`,
  `BridgeOut::Held` / `Resumed`, `ErrorCode::HoldFailed`; serde round-trip
  tests; PROTOCOL.md §3/§4 (incl. the peer-vs-bot hold contrast table).
  No behaviour yet.
- **Chunk 2 — the hold drive (the meat).** media-glue `generate_hold_offer`;
  generalise `TransferContext`→`DialogControl`; controller `Hold`/`Resume`
  arms (re-INVITE via `send_reinvite`, classify 2xx/491-glare/non-2xx),
  `held` state, reuse `TapCommand::Park`/`Unpark` for MOH (+ the
  drain-playout tweak); the `[media].moh_file` config; acks/errors.
- **Chunk 3 — observability + docs + SIPp + release.** `holds_total`
  metric + CDR `hold{count,total_ms}`; PROTOCOL/CONFIG docs + a bot-guide
  section (hold vs park, and §9 backend park-retrieve); the inverted-
  hold SIPp phase; CHANGELOG; version bump 0.7.2; tag.
