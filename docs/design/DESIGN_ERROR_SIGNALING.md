# Design: error-signaling — rtp_timeout / audio_format / protocol_error

Status: **DRAFT — decisions pending** (this note + an AskUserQuestion lock
the forks, then chunked PR(s), same cadence as prior themes).

Theme: **step 3** (final) of closing the protocol doc↔impl drift (bug #4).
Make the daemon actually *emit* the three remaining documented `error`
codes whose underlying conditions are already detected (or trivially
detectable) but which never reach the WS server. Step 1 corrected the
"can't happen" codes; step 2 implemented the liveness codes
(`server_too_slow`, keepalive `internal`). This closes the set.

---

## 1. The gap

Three documented `error` codes (PROTOCOL.md §3.10) are still never emitted:

- **`rtp_timeout`** — the media-glue inactivity watchdog
  (`[media].inactivity_timeout_secs`, default 60 s of no inbound RTP)
  already fires and tears the call down (`TapDisconnect::InactivityTimeout`
  → `CallTermination::TapEnded`). But the WS server is never told *why* —
  no `error { code: "rtp_timeout" }` is sent before the socket closes. The
  server just sees a bare close and has to guess.
- **`audio_format`** — PROTOCOL.md §2.2 says a binary frame of the wrong
  **size** (not the negotiated 20 ms — 320 B @ 8 kHz, 640 B @ 16 kHz) is
  rejected with this code. Reality: `conn.rs` forwards every inbound binary
  frame to media-glue unchecked. A server bug (e.g. 10 ms or 30 ms frames)
  is silently mis-injected.
- **`protocol_error`** — malformed JSON, an unknown message `type`, or a
  `call_id` that doesn't match the connection. Reality: `conn.rs` *detects*
  all three (`BridgeError::BadJson` / `CallIdMismatch`) and tears down — but
  returns a bare error and closes **without** emitting
  `error { code: "protocol_error" }` first, so the server never learns it
  sent something invalid.

The emit machinery already exists: `OutgoingEvent::{Error,Stop}` map
through `build_bridge_out`, and `conn.rs` has an `emit_fatal` helper (added
in step 2) that sends `error` + `stop` per the §3.10 invariant.

## 2. Design

### 2.1 `rtp_timeout` (cross-task, controller-driven)

The watchdog lives in media-glue; the WS sink lives in the conn task. The
controller (`core::call`) already bridges them via `control_out_tx`. On the
`tap_task` arm, when the result is `Ok(TapDisconnect::InactivityTimeout)`,
queue `OutgoingEvent::Error { code: RtpTimeout, .. }` then
`OutgoingEvent::Stop { reason: Error }` **before** `break`. Teardown then
`drop(control_out_tx)`; the conn drains both (biased recv), emits them, and
closes within the existing 250 ms budget. CDR termination stays
`TapEnded` (the *cause* is the RTP timeout; the stop is just the mechanism).
Not reconnect-related — this is a SIP/tap-side timeout, never the WS path.

### 2.2 `audio_format` (conn-local detection)

Capture the expected frame size once, before `start` is moved into the
`Start` message: `bytes = sample_rate/1000 * frame_ms * channels * 2`
(PCM16). In the inbound `Message::Binary` arm, compare `data.len()`.
Behavior on mismatch is **decision 1** below.

### 2.3 `protocol_error` (conn-local detection)

In the `Message::Text` arm, the `BadJson` and `CallIdMismatch` paths emit
`error { code: "protocol_error" }` + `stop` (best-effort) then close. This
is a **definitive** teardown — a server sending invalid frames is buggy, so
reconnecting to it just repeats the failure. Return a new
`DisconnectReason::ProtocolError` (NOT reconnect-eligible, mirroring
`ServerTooSlow`) rather than the current reconnect-eligible `Err`. Unknown
`type` handling is **decision 2** below.

## 3. Decisions — LOCKED

1. **`audio_format` = drop-and-continue.** Drop the wrong-size frame, emit
   `error{audio_format}` **non-fatal, rate-limited** (first occurrence +
   at most one/sec), keep the call up. A bridge shouldn't kill a live call
   over a malformed frame; persistent failure is still caught by the
   dead-air / rtp watchdog. `audio_format` moves into the §3.10 **non-fatal**
   set (alongside `*_failed`) — no `stop` follows it.
2. **Unknown `type` = strict fatal.** An unknown message `type` is a
   `protocol_error` (fatal teardown), same as malformed JSON. No two-stage
   parse needed — serde's tagged-enum decode already rejects both
   identically, so `BridgeError::BadJson` covers both.

(Engineering calls, not asked: `protocol_error` is a definitive,
non-reconnect-eligible teardown; `rtp_timeout` emits via the existing tap
teardown; the fatal `protocol_error` path uses the step-2 `emit_fatal`
helper.)

## 4. Chunks

1. **`protocol_error` + `audio_format`** (conn-local) — both in `conn.rs`,
   plus `DisconnectReason::ProtocolError` + `reconnect_eligible` arm; unit
   tests (bad JSON / mismatched call_id / wrong-size frame). Honors
   decisions 1 & 2.
2. **`rtp_timeout`** (controller) — emit on the `tap_task`
   `InactivityTimeout` arm in `core::call`; test via the existing tap
   harness (a call that goes RTP-silent → server sees `error{rtp_timeout}`
   + `stop`).
3. **Docs + release** — reconcile PROTOCOL.md §2.2 / §3.10 (fatal-vs-
   non-fatal classification per decision 1), CHANGELOG, tag. (May fold into
   the earlier chunks if small.)
