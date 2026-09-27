# Design note — call timeline events

Status: **implemented** (see the PR that adds this note). Requested
2026-09-27: *"LiveKit has a UI feature where you can look at a call and
it contains a timeline view — a waveform of the audio stream with the
barge-in start, end and duration mapped along it. Do we have all the
features in siphon-ai to build a similar UI?"*

Companion to `DESIGN_REVERSIBLE_BARGE_IN.md` (arbitration events) and
`DESIGN_RECORDING_COMPLIANCE.md` (the WAV this timeline is drawn over).

---

## 0. Scope, stated up front

SiphonAI does not build the UI and does not store the event stream.
That is consistent with the project's one rule: the daemon supplies
timeline-stamped **facts** over the WebSocket; the developer's server
owns the product view. The server already receives every event, keyed
by `call_id` and `seq`, and pairs a call with its recording through
`recording_id`.

So the question is only: *does the wire carry enough for a server to
draw the picture?* Before this note, almost. Three gaps:

1. **The WAV and the events were on two unanchored clocks.** `offset_ms`
   is zero at the instant the daemon sent `start`. The recorder starts
   on its own clock when the file opens — typically tens of milliseconds
   apart — and nothing stamped the difference.
2. **Some events had `seq` but no timeline position:** `mark`,
   `barge_in_resolved`, `hold`/`resume`, `held`/`resumed`, the recording
   lifecycle, and `stop`.
3. **Nothing said when the bot was talking.** A timeline has two lanes;
   the caller lane is fully covered by `speech_started`/`speech_stopped`,
   the bot lane had nothing. And in `auto_clear` mode a `speech_started`
   did not say whether it interrupted playout.

All three are closed by **additive** fields and one **opt-in** event
pair. Protocol stays `v1` (same precedent as `offset_ms` in 0.47.0).

---

## 1. `offset_ms` everywhere a moment is reported

Every daemon→server event that marks a *moment* now carries
`offset_ms` — monotonic milliseconds from the daemon's monotonic clock
between sending `start` and the moment, immune to wall-clock skew and
WS transit jitter. Already present on speech, DTMF, silence and
dead-air; added to:

| Event | The moment `offset_ms` names |
|---|---|
| `mark` | the estimated playout completion the mark fired at |
| `barge_in_resolved` | when the arbitration resolved |
| `hold` / `resume` (peer) | when the re-INVITE's direction change was applied |
| `held` / `resumed` (your request) | when the SIP round-trip completed |
| `recording_started` | **the recording file's first sample** — the anchor (§2) |
| `recording_stopped` / `recording_failed` | when the file was finalized / the failure hit |
| `stop` | the end of the timeline |
| `playout_started` / `playout_stopped` | see §3 |

Mechanically: each `OutgoingEvent` variant carries an `at: Instant`
stamped **at the source** (not at send), and the bridge connection
converts it against `start_sent` exactly as it already did for speech.
Stamping at the source matters where the event is delayed on its way
to the wire (a `held` waits on the SIP round-trip; a `mark` is armed
and fires later).

Absent from older daemons; SDKs type it optional.

## 2. Anchoring the WAV

`recording_started.offset_ms` is stamped when the writer's file opens,
which is the instant its first 20 ms frame is written (the writer's
tick starts immediately). So for a recording at `rate` Hz:

```
timeline_ms(sample s) = recording_started.offset_ms + s * 1000 / rate
```

and inversely, an event at `offset_ms` sits at sample
`(offset_ms - recording_started.offset_ms) * rate / 1000`. The stereo
layout is unchanged: left = caller, right = bot.

**Precision.** For `mode = "always"` the file opens just before the
writer's 20 ms interval starts, and its first tick fires immediately, so
the anchor is exact to the scheduler. For `on_demand`, the file opens
inside the running loop and its first frame lands on the next tick, so
the anchor can lead the first sample by up to one frame (20 ms). The
writer's interval uses `MissedTickBehavior::Skip`: a writer task starved
for more than a frame drops those ticks, and the file then runs short of
the timeline by the skipped frames. That does not happen at normal load,
and making the recorder pad skipped ticks is left as a follow-up.

**Pause caveat.** `pause_recording` omits the paused span from the
file, so the mapping above holds up to the first pause. The server is
the party that issued the pause and resume, so it can subtract its own
spans; a `recording_paused`/`recording_resumed` event pair would make
that daemon-stamped and is left for demand.

**Which frames reach the file** is unchanged: what the caller heard.
Fill frames (`idle_keepalive`), MOH and announcements are on the right
channel where audible; server audio the daemon discarded (peer hold,
mute, a `clear`) is not.

## 3. Bot turns: `playout_started` / `playout_stopped`

Opt-in via `[bridge].playout_events = true` (per-route override
`[route.bridge].playout_events`). Off by default: an unknown event type
is harmless to a v1 server (SDKs wrap it as `UnknownEvent`) but is
still traffic on every call, and most servers already know when they
sent audio. Servers that want daemon-stamped bot turns — the only
authoritative answer to "what did the caller actually hear, and when"
— turn it on.

```json
{ "type": "playout_started", "call_id": "...", "seq": 20, "offset_ms": 1180 }
{ "type": "playout_stopped", "call_id": "...", "seq": 31, "offset_ms": 4360, "duration_ms": 3180, "reason": "completed" }
```

A **turn** is a run of server audio reaching the caller. It **starts**
when the first frame of server audio is handed to forge after the
playout clock was idle (nothing queued, or the previous audio finished
more than `PLAYOUT_TURN_HANGOVER` = 250 ms ago). It **stops** with one
of:

| `reason` | When |
|---|---|
| `completed` | the queued audio finished and no new frame arrived within the hangover. `offset_ms` is the clock's estimated end of the last frame (not the timer's firing time); `duration_ms` = that minus the start. |
| `barge_in` | the caller's speech cut it: `auto_clear` flush, debounce-confirmed flush, or a pause-mode arbitration arming. On a **reject** the retained tail resumes as a **new** `playout_started` — truthful for a timeline (the caller heard a gap). |
| `cleared` | the server sent `clear` outside an arbitration. |
| `muted` | the server sent `mute`, which drops the queue. |
| `held` | a bot-initiated hold started (MOH replaces the bot). |
| `parked` | the call was parked. |

A cut that lands inside the hangover, after the audio already
finished, closes the turn `completed` at the audio's end: the caller
was not cut off, and a timeline should not draw it as an interruption.

Not a turn: MOH, announcements, `idle_keepalive` fill, conference mix —
none of those pass through the outbound queue's forge push, so they
never touch the clock. A turn open when the call ends is closed by
`stop` (no separate `playout_stopped`).

Why a 250 ms hangover: a real-time server stalls for 80–200 ms
routinely (GC pause, TTS chunk boundary), and the tap holds five frames
of lead, so a split needs a stall past ~350 ms. Same reasoning as the
idle-keepalive debounce in `tap.rs`, deliberately the same number.

Precision: `playout_started.offset_ms` is the forge hand-off of the
first frame; with the JIT lead the audio is on the wire within a frame
or two. Good enough for a waveform overlay, and honest about what it is.

**WebRTC legs** emit no tap events today (not even `mark`), so this
pair is likewise not emitted on them.

Observability: `siphon_ai_playout_turns_total{reason}` counts stops;
turn start/stop log at `debug` with `call_id`.

## 4. `speech_started.bot_playing`

`bot_playing: true` is present when the tap saw the bot in playout at
the moment of detection — the same `bot_is_playing` test that decides
whether `auto_clear` flushes, a debounce holds, or pause mode arms.
Absent (never `false`) otherwise, so old consumers see nothing new.

It is mode-neutral: with `auto_clear` it means "this speech cut the
bot off"; with `pause` it accompanies `decision_pending` (the two differ
only when a room or a still-pending verdict prevented arming); with
`notify_only` it tells the server the caller spoke over the bot and the
server chose not to have the daemon react. A timeline uses it to draw a
`speech_started` as an interruption rather than a turn-taking.

Stamped at detection, before any debounce hold, like `at`.

## 5. What this deliberately leaves out

- **Persisting the timeline in SiphonAI.** The CDR stays a summary
  (`barge_in_count`), HEP Log chunks stay lifecycle-only. A server or
  a webhook consumer that wants history stores the WS events.
- **Server-side (TTS) turn boundaries.** The server knows when it
  *sent*; `playout_*` says when the caller *heard*. Both are useful and
  they differ by the queue depth; a server that wants both drops a
  `mark` at each TTS boundary.
- **`recording_paused`/`recording_resumed` events** (§2 caveat).
- **Conference and WebRTC coverage** of the new event pair.

## 6. Compatibility

All new fields are optional-on-the-wire (`skip_serializing_if`) and the
new event pair is opt-in. `schemas/siphon-ai.v1.json` is regenerated;
both SDKs gain the fields and the two typed events; the conformance
corpus (every JSON example in `PROTOCOL.md`) covers them. Protocol
version unchanged.
