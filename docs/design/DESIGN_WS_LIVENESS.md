# Design: WS liveness — keepalive + start-deadline

Status: **DRAFT — decisions pending** (this note + an AskUserQuestion lock
the forks, then chunked PR(s), same cadence as prior themes).

Theme: step 2 of closing the protocol doc↔impl drift (bug #4). Implement the
two WS-liveness behaviors `docs/PROTOCOL.md` documents as **MUST** but the
daemon doesn't do — so a non-responsive WS server can no longer wedge a live
call. (Step 1, the "can't happen" doc corrections, shipped separately. Step 3
— `rtp_timeout` / `audio_format` / `protocol_error` error-signaling — is a
later theme.)

---

## 1. The gap

Two documented MUSTs are unimplemented; both let a broken WS server hold a
call open indefinitely:

- **WS keepalive (§5.6).** Spec: SiphonAI pings every 15 s and tears down
  with `error { code:"internal", message:"ws keepalive timeout" }` if the
  server doesn't pong within 10 s. Reality: `conn.rs` **pongs inbound pings
  but never sends its own** (the module doc admits "not yet implemented").
  The WS lib only surfaces an error when the **TCP** connection dies — so a
  **half-open** connection (TCP alive, server process hung; common behind
  NAT timeouts or a wedged event loop) is never detected and the call sits
  on silence forever.
- **Start-deadline / `server_too_slow` (§3.1).** Spec: the server MUST begin
  sending audio (or send `hangup`) within 5 s of `start`, else
  `error { code:"server_too_slow" }` + teardown. Reality: **no timer.** A
  server that connects, gets `start`, then never speaks holds the caller on
  comfort-silence until the RTP inactivity watchdog (`[media]
  .inactivity_timeout_secs`, default 60 s) or the caller hangs up.

Both behaviors live in one place: `run_loop`'s `tokio::select!`
(`crates/bridge/src/conn.rs:447`), which already relays control/audio and
pongs inbound pings.

## 2. Design

Both add a branch to the existing `run_loop` select; both emit the
documented fatal `error` + `stop { reason:"error" }` then close (matching
the §3.10 fatal-error contract). No new task, no shared state.

### 2.1 Keepalive

- A ping ticker (interval) fires every **ping interval**; each tick sends a
  `Message::Ping` and records "ping outstanding since `t`".
- Inbound `Message::Pong` clears the outstanding marker (and records
  last-seen). The daemon already pongs inbound pings — unchanged.
- A check (on each ping tick, or a second short timer) fails the call if a
  ping has been outstanding longer than the **pong deadline**:
  `error{internal,"ws keepalive timeout"}` + `stop` + return
  `BridgeError`. This makes the currently-dead `internal` code real.

### 2.2 Start-deadline (`server_too_slow`)

- A one-shot deadline armed when `start` is sent.
- **Disarmed** on the first inbound **audio** frame (`Message::Binary`) — or
  a `hangup` — from the server (see decision 2).
- On expiry: `error{server_too_slow}` + `stop` + return
  `Ok(DisconnectReason::ServerTooSlow)` (a definitive, non-reconnect-eligible
  teardown — redialing the same slow server wouldn't help).
- Armed on **every** WS session — initial, reconnect (0.7.3), and
  park-retrieve — since `run_loop` runs per session. A reconnected-but-silent
  server is as broken as an initial one, so it gets the same protection.

### 2.3 Relationship to the RTP watchdog (`rtp_timeout`)

Orthogonal. The media-glue inactivity watchdog (`[media]
.inactivity_timeout_secs`) watches **inbound RTP from the SIP peer** (caller
went away). This watches the **WS server** (bot went away/hung). Both can
fire independently; neither replaces the other. (`rtp_timeout`'s *WS error
emission* is step 3 — the watchdog already tears the call down.)

## 3. Decisions — LOCKED

1. **Configurable, spec defaults.** New `[bridge]` knobs:
   `ws_ping_interval_secs` (default 15), `ws_pong_timeout_secs` (default 10),
   `server_start_deadline_secs` (default 5). Defaults honor the spec MUSTs;
   operators can lengthen the start-deadline for slow-cold-start bots.
   **`server_start_deadline_secs = 0` disables** the start-deadline (escape
   hatch). The keepalive knobs follow suit (`0` on either disables keepalive).
2. **Disarm on first audio frame (strict).** Only an inbound binary audio
   frame (or `hangup`) clears the start-deadline, per the spec's literal
   "begin sending audio". A server that sends only control/setup but no audio
   within the deadline trips `server_too_slow`.

(Keepalive timeout uses `internal` per §5.6; keepalive applies to all WS
sessions; the start-deadline applies to the initial session.)

## 4. Chunks (target a patch/minor release)

Small + contained (one module, `conn.rs`):

1. **Implement** keepalive + start-deadline in `run_loop`; config plumbing if
   configurable (decision 1); unit tests with a mock WS server (hung server →
   keepalive timeout; silent-after-start → `server_too_slow`; healthy server
   → neither fires). Update `conn.rs`'s "not yet implemented" doc.
2. **Docs + release** — reconcile PROTOCOL.md §5.6 / §3.1 with the
   implementation (remove "not yet"/drift, document any new `[bridge]`
   knobs), CHANGELOG, tag. (May fold into chunk 1 if small.)
