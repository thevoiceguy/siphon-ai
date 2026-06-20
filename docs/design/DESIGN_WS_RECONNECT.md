# Design note — WS reconnect mid-call

> **Status: APPROVED — decisions LOCKED (§10, 2026-06-16).** Same
> design-first pass we did for park (`docs/design/DESIGN_0.7.0_PARK.md`) and hold
> (`docs/design/DESIGN_HOLD.md`), because this changes a published protocol
> contract (PROTOCOL.md §5.7, which today says reconnect is *not*
> supported) and touches the controller's teardown path. The build
> follows this; deviations get noted back here.

Today, if the WS connection to the developer's server drops mid-call,
the call **dies**: SiphonAI plays the fallback prompt, BYEs the caller,
and emits a CDR with `stop_reason = "ws_disconnect"` (PROTOCOL.md §5.7).
A server restart, a deploy, a brief network blip — any of them kills
every in-flight call. This note adds **opt-in automatic reconnect**: on
an unexpected WS drop, SiphonAI keeps the SIP call up on hold music,
re-dials the *same* `ws_url`, and resumes the call on a fresh WS session
keyed by the same `call_id` — falling back to today's teardown only if
reconnect can't be re-established within a bounded window.

This is the headline post-v1 item the dev plan explicitly defers and
flags as "worth gathering user feedback before designing"
(`docs/DEV_PLAN.md` §15.2; the `ws_reconnect_enabled = false` knob is
already sketched there).

---

## 1. Reconnect vs. the things it is *not*

| | Trigger | Caller during gap | WS session | Same `call_id`? | `seq` |
|---|---|---|---|---|---|
| **§5.7 teardown** (today) | any WS close before `stop` | fallback prompt → **BYE** | gone (call ends) | — | — |
| **park/retrieve** (0.7.0) | operator action | MOH | detached, reopened by operator | yes | resets to 0 |
| **reconnect** (this) | **unexpected** WS drop | MOH | auto-detached, **auto-redialed** | yes | resets to 0 |

Reconnect is, mechanically, **"auto-park on an unexpected drop +
auto-retrieve when the WS comes back, bounded by a deadline."** It reuses
park's MOH media path and park-retrieve's bridge-rebuild wholesale
(`call.rs` `ParkCommand::Retrieve` arm: fresh channels, fresh `start`,
swap `control_in_rx`/`control_out_tx` + the handle's bridge sender,
`TapCommand::Unpark`). The new work is the *policy* (when to reconnect,
backoff, when to give up) and a small bridge signal so the controller
knows the redial actually succeeded before it drops the MOH.

**SiphonAI is always the WS client** (it dials the developer's server),
so "reconnect" means *we re-dial* — never "the server reconnects to us."
The dev plan's old "if the WS reconnects within 2 s, resume" framing
(§272) assumed the inverse and does not fit our architecture; this
design supersedes it.

---

## 2. WS protocol additions (version stays `"1"` — additive)

- `start.reconnected: bool` — `true` on the `start` of a session that is
  resuming a call after an unexpected WS drop. **Absent** (not `false`)
  on a normal `start`, exactly like `retrieved` (§3.1). Distinct from
  `retrieved` on purpose: `retrieved` = "an operator picked up a call you
  parked"; `reconnected` = "your connection dropped and we re-dialed you
  for the same call." A server that ignores the field treats it as a
  brand-new call (degrades safely).
- **`stop_reason` / `StopReason`** — unchanged. On an unexpected drop we
  do **not** send `stop` on the dead session (the socket is already
  gone); the new session simply opens with `start.reconnected: true`. If
  reconnect ultimately fails, the existing `ws_disconnect` teardown (§5.7)
  runs as today.

**Server contract (documented, not enforced):** the same `call_id`
arriving on a fresh WS with `reconnected: true` means "tear down whatever
handler you still have for this call_id; this socket is now the live
one." Audio and events from the gap are **not** replayed — the server
resumes live (see §5).

*No new `BridgeIn`/`BridgeOut` message types.* Reconnect is driven
entirely by SiphonAI; the server's only visibility is the fresh
`start.reconnected` (and, if it cares, the gap in `seq` continuity — a
new session restarts `seq` at 0, preserving the "monotonic within a
session, never resets *within* a session" invariant of §5.2).

---

## 3. Detection & trigger — when do we reconnect?

The bridge task (`connect_and_run`) ends with a `DisconnectReason`
(conn.rs). Map each to an action **when `ws_reconnect_enabled`**:

| Bridge outcome | Today | With reconnect on |
|---|---|---|
| `StopSent` (we sent `stop`) | teardown | teardown — call is ending, never reconnect |
| server sent `hangup` then closed | teardown | teardown — explicit end |
| `ServerClosed` (close before `stop`) | §5.7 teardown | **reconnect** |
| connect error / IO error / TLS error | §5.7 teardown | **reconnect** |
| keepalive ping/pong timeout (§5.6) | teardown | **reconnect** |
| `ControllerHungUp` | teardown | teardown — we are ending it |

**The spicy decision (§10.5):** a bare WS close `1000` *without* a
preceding `hangup` is treated as an **unexpected drop → reconnect**, not
as "the server is done." Rationale: in the protocol, the way a server
ends a call is the `hangup` control message (§4); a socket close is
ambient transport, not an end-of-call signal. This is a **behavior
change** for servers that today end calls by just closing the WS — but
it's gated entirely behind the (default-off) enable flag, so v1 behavior
is unchanged unless an operator opts in. Documented loudly in
PROTOCOL.md.

Reconnect is **suppressed** when the call is already parked (no WS to
drop) or mid-teardown.

---

## 4. Reconnect policy — backoff & giving up

- **`[bridge].ws_reconnect_enabled`** (bool, default **false**) — opt-in,
  like every feature since 0.5.0. Per-route override via
  `[route.bridge]` (same merge rules as the other bridge knobs).
- **`[bridge].ws_reconnect_max_secs`** (default **30**) — the total
  wall-clock window the caller may spend on reconnect MOH. When it
  elapses without a successful redial, fall through to §5.7 teardown
  (fallback prompt → BYE → CDR). Bounding by *time* rather than attempt
  count keeps the caller experience legible ("hold music for up to N s,
  then we hang up").
- **Backoff schedule:** internal, fixed first cut — exponential with
  jitter, 250 ms → ×2 → cap 5 s, redialing until either a redial
  succeeds or `ws_reconnect_max_secs` elapses. Kept out of config to
  avoid sprawl (§10.2); revisit if operators need to tune it.
- **Composition with `on_ws_failure`:** reconnect runs *first*; only on
  exhaustion does the existing `on_ws_failure` path (today `"hangup"`)
  run. They stack cleanly — reconnect is the resilience layer in front of
  the terminal failure handler.

---

## 5. Media during the gap

Reuse park's media path: on the drop, `TapCommand::Park { moh }` — the
tap stops forwarding caller→WS audio, stops the direct playout pair, and
plays `MohSource` on the 20 ms tick. The caller hears hold music for the
duration of the reconnect window instead of dead air.

- **MOH source:** the shared `[media].moh_file` (added in 0.7.2 for
  hold), comfort-noise fallback when unset / rate-mismatch — identical to
  park and hold. No new config.
- **Audio in the gap is lost, not buffered.** We do not buffer caller
  RTP (50 frames/s for an unbounded gap is unbounded memory) and we do
  not replay it on reconnect (stale audio is worse than a clean gap). The
  caller's words during the outage are gone — same as a real hold.
- **On successful redial:** `TapCommand::Unpark { … }` with the fresh
  bridge's channels restores the direct caller↔WS pair — the exact
  park-retrieve swap.

---

## 6. Controller lifecycle

The bridge-end arm today is `BridgeEnded → break` unless `parked`
(call.rs). Generalise it: on an **eligible** unexpected drop (§3) with
reconnect enabled and within the deadline, enter a `reconnecting` state
instead of breaking.

New state (alongside the existing `parked` / `bridge_alive`):
- `reconnecting: bool`, `reconnect_attempt: u32`, `reconnect_since:
  Option<Instant>` (for the CDR gap accounting).
- A pinned `reconnect_backoff_sleep` and a pinned `reconnect_deadline`
  (same far-future-pinned-`Sleep` pattern as the park timeout).

Flow:
1. **Eligible drop** → `tap_cmd_tx.send(Park{moh})`, `reconnecting =
   true`, arm `reconnect_deadline = now + ws_reconnect_max_secs`, arm the
   first backoff. (The bridge-end arm is already guarded by
   `bridge_alive`; reuse that so the completed handle isn't re-polled.)
2. **Backoff fires** → rebuild fresh channels + spawn a fresh
   `connect_and_run` with `start { reconnected: true, seq: 0 }` — verbatim
   the park-retrieve rebuild. **Do not `Unpark` yet** (§6.1).
3. **Redial succeeds** → drop MOH (`TapCommand::Unpark`), `reconnecting =
   false`, account the gap, bump the metric, resume normally.
4. **Redial fails** (bridge ends again fast, before ready) → bump
   attempt, re-arm a longer backoff, stay on MOH. Loop until the deadline.
5. **Deadline fires while still reconnecting** → fall through to §5.7
   teardown (`StopReason` path / `on_ws_failure`).

### 6.1 Knowing the redial actually connected (§10.6)

`connect_and_run` today doesn't signal "connected" separately from
"running" — it does the handshake, sends `start`, then runs. To avoid
flapping MOH off before we know the new socket is healthy, add an
optional readiness signal: `connect_and_run(..., ready_tx:
Option<oneshot::Sender<()>>)` fired **after** the WS handshake succeeds
and `start` is written. The controller waits on it during reconnect:
keep MOH until ready, then `Unpark`. A bridge that ends without ever
firing `ready` is a failed redial → backoff + retry. This is a small,
generally-useful bridge addition (the initial connect path could expose
it too). *Alternative (§10.6):* optimistic — `Unpark` immediately on
redial and re-`Park` if it drops again; simpler but flaps the caller's
audio on a flaky link. **Recommend the ready signal.**

---

## 7. Interactions & edge cases

- **Parked call** → no WS to drop; reconnect N/A (park already detached
  the session; an operator retrieve is the only way back).
- **Held call** (0.7.2) → the WS is open, so a drop *is* eligible. The
  controller's `held` state persists across the reconnect; the caller is
  already on MOH from the hold, so reconnect's MOH is a no-op overlap.
  The fresh `start` does **not** currently carry "you are held" — first
  cut: the controller stays `held`, and the server may re-issue `resume`
  when it wants two-way audio back (§10.8). A `start.held` hint is a
  later refinement.
- **Conference** → the SIP leg stays mixed in the room while the WS is
  down (the room is tap-side state, independent of the WS). Reconnect
  restores the WS control channel, but the room membership and the fresh
  `start` interaction need care — **first cut: document that a call in a
  room reconnects its control channel but does not auto-rejoin** (the bot
  re-issues `conference_join` if needed), or disallow reconnect for
  in-room legs. Pin in §10.8.
- **Transfer in flight** → the REFER is a spawned task independent of the
  WS; a drop during it doesn't abort the REFER. If the REFER completes
  (call ends) mid-reconnect, teardown wins over reconnect.
- **Recording** → keeps writing through the gap (captures the MOH),
  consistent with park/hold ("what the caller heard").
- **Repeated flapping** → each drop within the same call increments the
  reconnect counter; the deadline is **per drop** (re-armed on each
  successful resume), so a call that flaps every minute reconnects each
  time rather than exhausting a lifetime budget. (Alternative: lifetime
  budget — §10.2.)
- **Auth** → each redial re-sends `auth_header` verbatim, same as the
  initial connect. A server that rotated a token mid-call and now rejects
  the redial → reconnect fails → teardown.

---

## 8. Observability

- **Metric:** `siphon_ai_ws_reconnects_total{result=recovered|exhausted}`
  — one increment per reconnect *episode* (a drop that entered the
  reconnect path), `recovered` = redialed within the window, `exhausted`
  = hit the deadline and tore down. Optional gauge
  `siphon_ai_ws_reconnecting_active` (calls currently on reconnect MOH).
- **CDR (additive, schema stays v1):** `reconnect { count, total_gap_ms }`
  — mirror `park`/`hold` accounting (`crates/cdr/src/schema.rs`
  `ParkInfo`/`HoldInfo` → a parallel `ReconnectInfo`). `count` = reconnect
  episodes over the call, `total_gap_ms` = summed time on reconnect MOH.
  Omitted when the call never reconnected.
- **Webhook (§10.7):** a WS flap is arguably a lifecycle event an
  out-of-band consumer wants (it signals server-side instability) —
  unlike hold, which is a transient in-call action. *Recommend deferring*
  to keep the first cut to metric + CDR + logs; revisit if operators ask.
- **Logs:** `ws bridge dropped; reconnecting (attempt N)` at `warn`, `ws
  reconnected after {gap}ms` at `info`, `ws reconnect exhausted after
  {max}s; tearing down` at `warn` — all with `call_id` in the span.
- **HEP:** none directly — this is a WS-transport event, not SIP/RTCP.
  The SIP leg's BYE on exhaustion still ships via the existing SIP HEP
  path.

---

## 9. Integration regression

No new SIPp scenario is strictly needed for the SIP side (the dialog is
untouched — caller stays connected throughout). The reconnect behavior is
exercised by a **harness phase** like the others: an echo-ws started with
a new `--drop-after-ms` knob (closes the socket mid-call once), with the
daemon configured `ws_reconnect_enabled = true`. Assertions:

- the SIP caller (`basic_call_then_bye.xml`) stays up across the drop
  (no BYE during the reconnect window);
- the echo-ws receives a **second** `start` carrying `reconnected: true`;
- `siphon_ai_ws_reconnects_total{result="recovered"}` reads 1;
- a second phase with the echo-ws never coming back asserts
  `result="exhausted"` + the caller gets the BYE after
  `ws_reconnect_max_secs`.

The echo-ws gains the `--drop-after-ms` knob and (for the recovered case)
a relisten/accept of the redial.

---

## 10. Decisions — LOCKED (2026-06-16)

1. **Enable flag** — **off by default.** `[bridge].ws_reconnect_enabled`
   opt-in, like every theme since 0.5.0; v1 §5.7 behavior is unchanged
   unless an operator enables it. §4.
2. **Bounding** — **per-drop total-time deadline.**
   `[bridge].ws_reconnect_max_secs` (default 30) with an **internal fixed
   backoff** schedule (250 ms → ×2 → cap 5 s; no backoff config). The
   deadline re-arms on each successful resume, so a flapping call
   reconnects each time rather than burning a lifetime budget. §4, §7.
3. **Resume flag** — **new `start.reconnected: bool`**, omitted-when-false
   like `retrieved`; *not* a reuse of `retrieved` (distinct semantics —
   operator-retrieve vs. transport-redial). §2.
4. **Gap media** — **MOH**, reusing the shared `[media].moh_file` (comfort
   noise fallback). Gap audio is lost, never buffered or replayed. §5.
5. **Bare WS close (1000) without `hangup`** — **treated as an unexpected
   drop → reconnect.** To *end* a call the server sends `hangup`; a socket
   close is transport, not an end-of-call signal. This is a behavior
   change for servers that end calls by closing the socket, but it is
   gated entirely behind the default-off flag and documented loudly in
   PROTOCOL.md §5.7 + the bot guide. §3.
6. **Redial confirmation** — **add the `connect_and_run` readiness
   signal** (`ready_tx` oneshot fired after handshake + `start`); the
   controller keeps MOH until ready, then `Unpark`. No optimistic flap.
   §6.1.
7. **Webhook** — **deferred.** First cut ships metric + CDR + logs; a
   `ws_reconnected` lifecycle webhook is revisited if operators ask. §8.
8. **Held / conference interactions** — **reconnect is orthogonal to
   `held`** (controller state persists across the redial; the server
   re-issues `resume` if it wants two-way audio). **In-room legs reconnect
   the control channel but do not auto-rejoin the room** — the bot
   re-issues `conference_join` if needed. Both refined later if real usage
   demands. §7.
9. **Version** — **0.7.3** (additive, off by default, protocol stays
   `"1"`) — a resilience patch theme, not 0.8.0.

---

## 11. Implementation chunks

Mirrors the park / hold / outbound-SRTP cadence — plan PR, chunked impl
PRs, then harness + release.

- **Plan** (this note).
- **Chunk 1 — config + protocol surface + bridge readiness.**
  `[bridge].ws_reconnect_enabled` + `ws_reconnect_max_secs` (+ per-route
  override, validation); `start.reconnected` field + serde tests +
  PROTOCOL.md §3.1/§5.7 rewrite (§5.7 stops saying "not supported");
  `connect_and_run` gains the optional `ready_tx` signal (wired but no
  reconnect behavior yet). No reconnect drive.
- **Chunk 2 — the reconnect drive (the meat).** Controller bridge-end arm
  classifies the `DisconnectReason`, enters `reconnecting`, drives MOH +
  backoff + redial reusing the park-retrieve rebuild and the ready
  signal, with the deadline fall-through to §5.7. `held`/parked/conference
  guards (§7).
- **Chunk 3 — observability + docs + harness + release.**
  `ws_reconnects_total` metric + CDR `reconnect{count,total_gap_ms}`;
  CONFIG/PROTOCOL docs + a bot-guide note ("ending a call: send `hangup`,
  don't just close the socket"); the `--drop-after-ms` harness phase
  (recovered + exhausted); CHANGELOG; version bump 0.7.3; tag.
