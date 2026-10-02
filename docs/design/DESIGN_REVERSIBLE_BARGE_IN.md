# Design note — reversible (server-arbitrated) barge-in

> **Status: APPROVED — decisions LOCKED (§11, all per recommendation,
> 2026-07-14).** Pins the WS surface, the
> media mechanics, and the interaction matrix before implementation — the
> same design-first pass as hold (`DESIGN_HOLD.md`) and park
> (`DESIGN_0.7.0_PARK.md`). The build follows this; deviations get noted
> back here. Target: **0.32.0**, protocol stays `"1"` (additive).

Makes barge-in a **provisional, reversible** action that the WS server
arbitrates, instead of an instant irreversible flush. Today a cough, a
backchannel ("uh-huh"), or background noise that gets past VAD kills the
bot's playout (`auto_clear`) or forces the server into a
latency-vs-talking-over-the-caller dilemma (`notify_only`). The fix:
SiphonAI reacts **instantly but reversibly** (pause playout, retain the
audio), and the server — the only layer with STT and conversational
context — confirms or rejects the barge-in within a bounded window. A
false positive costs a sub-second pause instead of a killed utterance.

**What this is *not*** (scope fence):

- **Not intent detection in the daemon.** Distinguishing "uh-huh" from
  "yeah, but—" needs a transcript; that is the WS server's job
  (CLAUDE.md §4.1). SiphonAI supplies the reversible mechanism and the
  signals; the server supplies the verdict.
- **Not a VAD upgrade.** A neural VAD (Silero-class) in `forge-vad` is a
  separate, upstream-gated track that attacks the *acoustic* false-positive
  class (coughs, noise). Complementary, not a dependency.
- **Not AEC.** Echo-driven false positives are already mitigated by the
  playout-gated `debounce_ms` (0.7.x, #173), which composes with this
  design (§7.1); libwebrtc APM in forge-media remains the heavier fallback.

---

## 1. The new mode vs. the things it is not

`[bridge.barge_in].mode` grows a third value, `"pause"`:

| | On `speech_started` while bot is playing | Reversible | Who decides | False-positive cost |
|---|---|---|---|---|
| **`auto_clear`** (default, exists) | flush playout immediately | no | SiphonAI (VAD *is* the decision) | utterance killed |
| **`notify_only`** (exists) | nothing — server may send `clear` | n/a | server, but reaction waits a full round-trip | bot talks over the caller |
| **`pause`** (this) | **pause playout instantly, retain unplayed audio, await verdict** | **yes** | server (bounded by `decision_ms`, then `on_timeout`) | sub-second dip in the bot's speech |

While the bot is **silent**, `pause` behaves exactly like the other modes:
`speech_started` is forwarded and nothing else happens — there is nothing
to pause, so no arbitration is armed. Arbitration exists only to protect
in-flight playout.

The verdict outcomes:

- **confirm** → the retained audio is dropped; identical end-state to
  today's `auto_clear` flush. Counted as a real barge-in.
- **reject** → the retained audio is re-queued and playout resumes where
  it stopped (biased to repeat ~1 frame rather than skip — §5.3).
- **timeout** (`decision_ms` elapses with no verdict) → apply
  `on_timeout` (default `"confirm"` — fail toward not talking over the
  caller). A server that never arbitrates therefore degrades to
  "auto_clear delayed by `decision_ms`", which is safe and predictable.

Caller→server audio is **never** touched by arbitration — the server
needs it for the STT that produces the verdict.

---

## 2. WS protocol additions (version stays `"1"` — additive)

Per the CLAUDE.md §7.1 checklist: `BridgeIn`/`BridgeOut` variants +
PROTOCOL.md + schema regen + both SDKs + example servers + testkit
scenario, all in the same PR chain.

- **`BridgeIn::BargeInConfirm { call_id }`** — verdict: real barge-in.
  Drop the retained audio, stay quiet. No-op (debug log, no error) when no
  arbitration is pending — verdicts race with `speech_stopped` and with
  the deadline by nature, so a late verdict must be harmless.
- **`BridgeIn::BargeInReject { call_id }`** — verdict: not a barge-in
  (cough / backchannel / noise). Resume the retained playout. Same no-op
  semantics when nothing is pending.
- **`clear` while an arbitration is pending acts as confirm** (documented
  in §4.1 of PROTOCOL.md) — it already means "drop pending playout", and
  this keeps mode-oblivious servers coherent.
- **`speech_started` gains an optional field** — in `pause` mode, when
  arbitration was armed:

  ```json
  { "type": "speech_started", "call_id": "...", "seq": 42, "ts_ms": 1234,
    "decision_pending": true, "decision_deadline_ms": 500 }
  ```

  Absent (not `false`) in every other case, so existing consumers and the
  schema's `additionalProperties` posture are untouched. The event *is*
  the arbitration request — no separate request message.
- **`BridgeOut::BargeInResolved { call_id, seq, outcome }`** with
  `outcome ∈ "confirmed" | "rejected" | "timeout"` — emitted when an
  armed arbitration resolves, whatever resolved it. The server mostly
  knows the outcome (it sent the verdict), but `timeout` is exactly the
  case it *doesn't* know about, and a single uniform event keeps SDK
  state machines and the conformance testkit honest. *(Decision §11.5.)*
- **`start` gains an optional `barge_in_mode` field**
  (`"auto_clear" | "notify_only" | "pause"`, the per-route resolved
  value) so SDKs can discover whether verdicts are expected instead of
  requiring out-of-band config agreement. *(Decision §11.8.)*

Naming mirrors the hold precedent (`DESIGN_HOLD.md` §2): imperative
request verbs, past-tense resolution event, explicit rather than
overloaded (`barge_in_confirm`, not a repurposed `clear` — though `clear`
aliases to confirm for compatibility).

SDK surface: `call.barge_in_confirm()` / `call.barge_in_reject()` in
both `sdks/python` and `sdks/typescript` (their tests assert full
coverage of the schema unions, so CI enforces lockstep), plus typed
`BargeInResolved` / extended `SpeechStarted` events.

---

## 3. Why "pause", not "duck" — the forge queue reality

The obvious alternative — attenuate ("duck") the bot instead of pausing —
**cannot be done tap-side today**. The tap pushes playout frames into
forge eagerly (`send_audio`, `tap.rs` playout arm); the queued tail —
potentially an entire TTS utterance — lives in **forge's encoder queue**,
which the tap can flush (`handle.flush(MediaTarget::A)`) but cannot
rewrite. Gain applied to *newly pushed* frames would leave seconds of
full-volume audio already in flight. A true duck needs an upstream
`forge-media` per-leg playout-gain API — a fine future variant
(*upstream-gated*, same shape as the AMD dependency), but not v1.

Pause has no such dependency, because **flush is instant and we can keep
our own copy** (§5). It is also arguably the better conversational UX for
a voice bot: going quiet the moment the caller speaks (as a human does),
then resuming if it was nothing, reads more naturally than continuing to
talk underneath them at low volume.

---

## 4. Config surface

```toml
[bridge.barge_in]                # global; [route.bridge.barge_in] mirrors it
enabled     = true               # existing
mode        = "pause"            # NEW value; "auto_clear" stays the default
debounce_ms = 120                # existing echo/noise gate — composes, §7.1
decision_ms = 500                # NEW — server verdict deadline (pause mode only)
on_timeout  = "confirm"          # NEW — "confirm" | "reject"
resume_max_secs = 30             # NEW — retained-audio cap per call (§5.4)
```

Validation at load (CLAUDE.md §4.6): `decision_ms` required > 0 when
`mode = "pause"` (`0` is rejected — an unbounded pause would hang playout
on a lost verdict); `on_timeout`/`resume_max_secs` rejected unless mode
is `pause`; unknown mode strings fail loud. Per-route override inherits
field-wise from the global block, same merge as today (`raw.rs`
`RawBargeIn`). All new fields documented in `docs/CONFIG.md` +
`docs/PROTOCOL.md` in the same PR.

Default posture: **`auto_clear` remains the default mode** — `pause` is
opt-in, consistent with every recent feature.

---

## 5. Media mechanics — the tap

All inside `crates/media-glue/src/tap.rs`, which already owns the flush,
the debounce, and the playout-clock estimation. Hot-path rules
(CLAUDE.md §4.3) hold throughout: no steady-state allocation, no locks,
no blocking, verdicts arrive via the existing `TapCommand` channel.

### 5.1 Shadow ring (the resume buffer)

In `pause` mode the playout arm copies each frame it pushes to forge into
a **preallocated ring buffer** (allocated once at `attach` when the
resolved mode is `pause`; other modes pay nothing). Frames are evicted
as the existing playout clock (`playout_until` / `frames_sent_to_forge`
cursor math, the same estimation that drives `Mark` and
`bot_is_playing`) says they have played out. Steady-state cost: one
~640-byte memcpy per 20 ms frame plus cursor arithmetic.

### 5.2 Pause (arming arbitration)

On a `speech_started` that passes the debounce gate while
`bot_is_playing` and mode is `pause`, run **exactly today's `auto_clear`
flush** — drain the controller→tap channel, `clear_ws_input()` if in a
room, `flush(MediaTarget::A)`, reset Mark bookkeeping — with two
differences:

1. The drained-channel bytes and the shadow ring's unplayed tail are
   spliced (shadow first, then drained bytes) into the **resume buffer**
   instead of being dropped.
2. A `decision_deadline` sleep is armed (same pinned-placeholder pattern
   as the debounce timer) and the forwarded `speech_started` carries
   `decision_pending: true`.

The caller hears the bot stop within one frame — the same reaction time
as `auto_clear`.

### 5.3 Resolve

- **Confirm** (verdict, `clear`, or timeout with `on_timeout="confirm"`):
  drop the resume buffer, bump `barge_in_count`, `publish_quality()`,
  emit `barge_in_resolved`. End-state identical to today's `auto_clear`.
- **Reject** (verdict, or timeout with `on_timeout="reject"`): re-push
  the resume buffer into forge (a burst push, which forge already
  absorbs — it's how normal TTS arrives), restore the Mark/playout-clock
  bookkeeping from the re-pushed frame count, emit `barge_in_resolved`.
  The playout-clock estimate carries ±1–2 frames of slop, so the splice
  point is biased **early** — repeating ≤40 ms is inaudible; skipping a
  syllable is not.
- New WS audio arriving from the server **while paused** queues behind
  the resume buffer in the normal controller→tap channel; on reject it
  plays after the resumed tail, on confirm it plays immediately (the
  server barged over itself — its choice).
- **The resolution's backlog is exempt from the §5.5 window (#620,
  fixed after 0.55.0).** The tail and the post-pause audio are re-pushed
  straight into forge, so forge then holds far more than its 5-frame
  lead. A server still streaming at real time accumulates in the
  outbound queue at one frame per frame played, and the fixed 10-frame
  window used to evict about (pause − 200 ms) of bot audio per
  resolution. A real PSTN call lost 158 frames over 5 rejects. The queue
  now carries an *allowance*, granted at every resolution (reject,
  confirm, and both timeouts) as `repushed − FORGE_LEAD_FRAMES`. It only
  shrinks, to `unplayed + held − FORGE_LEAD_FRAMES`, as that backlog
  really drains, and every flush resets it. A real-time server keeps the
  backlog level, so the allowance holds for the rest of the turn; an
  over-rate server is still bounded, because traffic never raises the
  allowance. Regression: `realtime_stream_through_pause_reject_loses_nothing`
  (19 of 150 frames lost without the fix).

### 5.4 Bounds

The shadow ring is capped at `resume_max_secs` (default 30 s ≈ 960 KB at
16 kHz — per-call, only in `pause` mode). On overflow the oldest frames
are evicted and a once-per-call `warn!` is logged; a reject then resumes
from the earliest retained frame (bounded content loss at the seam,
which only occurs on pathological >30 s single utterances).

---

## 6. Core / controller wiring

- `BargeInAction` (`tap.rs`) gains a third variant, `Pause { decision:
  Duration, on_timeout: TimeoutVerdict, resume_max: Duration }`; the
  acceptor resolves it from config exactly as it does the current two
  (`acceptor.rs` call-setup translation, per-route override included).
  *(Chunk-1 deviation noted: `resume_max` rides on the variant — it's
  the natural carrier down to the tap. Also, the shadow ring is a
  `VecDeque` of the already-owned frame `Vec`s — the frame buffers are
  moved in, not copied, so §5.1's "one memcpy per frame" became zero
  copies.)*
- Two new `TapCommand`s — `BargeInConfirm` / `BargeInReject` — routed
  from the `BridgeIn` match arm in `core/src/call.rs`, same shape as
  `Clear`/`Mute`. Verdicts for a call with no pending arbitration are
  dropped with a `debug!` (§2 no-op semantics).
- Outbound calls need nothing special — same tap, same policy resolution
  (barge-in is already direction-agnostic).

---

## 7. Interaction matrix

### 7.1 `debounce_ms` (composes — two different filters)

The debounce gate answers "was that even speech, or the bot's own echo?"
(acoustic, ~100–200 ms); arbitration answers "was that speech an
interruption?" (semantic, ~500 ms). In `pause` mode the debounce runs
**first**, unchanged: a provisional `speech_started` held by the gate
neither pauses nor forwards; only a debounce-confirmed barge-in triggers
the pause + arbitration. Echo blips therefore never reach the server at
all, exactly as with `auto_clear` today.

### 7.2 Everything else

| Feature | Interaction |
|---|---|
| `mute` / bot-`hold` / park / announce | Playout is already suppressed or preempted → `bot_is_playing` is false or the WS pair is detached → arbitration never arms. A `Park`/`Hold` command **during** a pending arbitration resolves it as confirm (drop the buffer) before the mode switch, mirroring how announce/park already preempt playout. |
| WS reconnect (0.7.3) | WS drop during a pending arbitration → resolve as **confirm** immediately (the arbiter is gone; flushed-and-quiet is the safe state alongside the reconnect MOH). Fresh session starts with no arbitration state. |
| Conference rooms (0.7.0) | Arbitration is **suspended while in a room** (`JoinRoom` resolves any pending arbitration as confirm; in-room `speech_started` behaves as `notify_only`). Multi-party barge-in semantics are genuinely different — out of scope for v1. *(Decision §11.7.)* |
| `Mark` | Marks pending at pause time fire per existing `Clear` semantics (the flush is real). Marks queued after a reject re-anchor on the restored clock. Documented in PROTOCOL.md §4.2. |
| Quality/CDR `barge_in_count` | Counts **confirmed + timeout-confirmed** resolutions (a rejected arbitration was, by the server's own ruling, not a barge-in). |

---

## 8. Observability (same PR as the feature — CLAUDE.md §4.5)

- **Metrics** (`telemetry/src/metrics.rs`, documented in `DEPLOY.md`):
  - `siphon_ai_barge_in_decisions_total{outcome="confirmed"|"rejected"|"timeout"}`
  - `siphon_ai_barge_in_decision_seconds` histogram (arm → resolve;
    explicit buckets around the expected 50 ms–1 s range).
- **CDR** (`cdr/src/schema.rs` `quality` block): additive **optional**
  fields `barge_in_rejected_count`, `barge_in_timeout_count` alongside
  the existing `barge_in_count`. Additive-optional → **no CDR version
  bump** (stays v4), per the §7.7 policy.
- **Logs**: `debug!` on arm/verdict/timeout with `call_id` span field;
  the existing once-per-call `warn!` pattern for ring overflow.
- **Live quality feed**: `publish_quality()` on every resolution (the
  watch already publishes on playout clears).
- **Traces**: span events `barge_in.pause` / `barge_in.resolved` on the
  call span (joins the OTLP per-call trace from 0.22.0).

---

## 9. Testing

- **Unit (tap)**: pause→confirm ≡ auto_clear end-state; pause→reject
  resumes with early-biased splice; timeout applies `on_timeout`;
  ring eviction under overflow; debounce-then-arbitrate ordering;
  verdict-with-nothing-pending is a no-op; room/park/hold/WS-drop
  resolutions per §7.2. Existing `tests/tap.rs` fixtures extend.
- **Protocol**: serde round-trips for the two `BridgeIn` verbs, the
  extended `speech_started`, `barge_in_resolved`, `start.barge_in_mode`;
  schema regen + docs-corpus validation (CI enforces).
- **SDKs**: both suites' union-coverage assertions force the new types;
  add verdict-method tests.
- **Conformance testkit**: new bundled scenario — server receives
  `decision_pending`, sends reject, asserts `barge_in_resolved
  {outcome:"rejected"}` and that audio resumes; a timeout variant
  asserts `outcome:"timeout"`. Runs against both echo servers in the
  `conformance` CI job (echo servers gain a trivial arbitration hook).
- **SIPp / harness**: one scenario with injected caller audio mid-playout
  verifying the pause-resume path end-to-end against the Python echo
  server (extends the existing barge-in phase).

---

## 10. Delivery plan (usual cadence)

1. **Chunk 1 — media mechanics**: shadow ring, `BargeInAction::Pause`,
   `TapCommand` verdicts, timeout, interaction resolutions (§7.2), tap
   unit tests. No protocol surface yet — inert without config.
2. **Chunk 2 — protocol + config + wiring**: config fields + validation,
   acceptor resolution, `BridgeIn`/`BridgeOut` variants, core match arms,
   schema regen, PROTOCOL.md + CONFIG.md, both SDKs, echo-server
   arbitration hooks, testkit scenarios, metrics/CDR fields.
3. **Chunk 3 — release 0.32.0**: changelog, version-consistency gate,
   SIPp scenario, tag per `RELEASING.md`.

---

## 11. Decisions to lock

1. **Mode name `"pause"`** (vs `"arbitrated"`, `"duck"`). *Recommend
   `pause`* — it names what the caller hears; the config grammar stays
   "what SiphonAI does alongside the event".
2. **Reaction = flush-with-shadow pause**, not gain-duck. *Recommend
   pause* (§3 — duck is upstream-gated on a forge playout-gain API; keep
   it as a possible future mode value, not v1).
3. **Defaults: `decision_ms = 500`, `on_timeout = "confirm"`.**
   *Recommend as stated* — 500 ms covers STT-partial latency for the
   major engines with margin; timeout-confirm fails toward silence.
4. **Explicit verdict verbs + `clear`-as-confirm alias** (vs overloading
   `clear`/`mark`). *Recommend explicit pair* — symmetric, self-listing
   in the schema, and `reject` has no plausible overload anyway.
5. **Emit `barge_in_resolved` for every resolution** (vs only timeout,
   vs nothing). *Recommend yes* — one uniform event; timeout is
   unknowable server-side without it.
6. **No local auto-verdict on `speech_stopped`** within the window —
   a short burst may be "stop!", which STT will catch; the server (or
   the deadline) always rules. *Recommend yes (no local verdict) for v1*;
   revisit if verdict latency proves annoying on real calls.
7. **Rooms suspend arbitration** (§7.2). *Recommend yes* — define
   multi-party semantics when conference demand exists.
8. **`start.barge_in_mode` announce field.** *Recommend yes* — additive,
   and SDKs otherwise can't know whether verdicts are expected.
9. **`resume_max_secs = 30` default, evict-oldest + warn on overflow**
   (vs degrade-to-confirm). *Recommend as stated* — graceful degradation
   preserves the recent tail, which is what a resume needs.
