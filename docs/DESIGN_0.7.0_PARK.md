# Design note — 0.7.0 chunk 4: Park + Retrieve

> **Status: IMPLEMENTED.** The build followed this design; the locked
> decisions (§12) all held. A handful of small deviations surfaced
> during implementation — see §13. This note now documents what shipped.

Implements DEV_PLAN_0.7.0.md §2.4 (park, media-only, §9.3). Park shelves
a call without a WS session — the caller hears hold music, the SIP dialog
and RTP stay up, and the call is later **retrieved** onto a *fresh* WS
session. This is the one chunk that reworks the per-call controller
lifecycle, so it gets a design pass.

---

## 1. What changes vs. what doesn't

**Unchanged, durable across a park:** the SIP dialog, the forge media
session + RTP, the `MediaTap` task (it keeps the forge `MediaBridgeHandle`
alive — that's *why* the tap must persist), the `CallController` task, the
call's entry in `CallRegistry` / `CallControlRegistry`, recording (a
recording started before park keeps writing — see §9).

**Detached on park, fresh on retrieve:** the WS `bridge` task and all four
of its channels.

**Out of scope (restating locked decisions):** no SIP re-INVITE/hold (the
dialog stays `sendrecv` and we keep sending RTP — MOH needs it); retrieve
is **operator-initiated only** (admin API), a *fresh* session (new `seq`
from 0, `start.retrieved: true`, no replay) — explicitly **not** mid-call
WS reconnect; no per-slot MOH.

---

## 2. Config — `[park]`

```toml
[park]
enabled = false                      # fail-closed, like [conference]/[outbound]
moh_file = "/etc/siphon-ai/moh.wav"  # optional; comfort noise if unset
timeout_secs = 300                   # 0 = no timeout
timeout_action = "hangup"            # "hangup" | "keep"
max_parked = 32
```

Validated at load (`compile_park`): `timeout_action ∈ {hangup, keep}`;
`max_parked ≥ 1`; if `moh_file` set, it must exist **and decode** (open it
through `forge_injection::FileSource` once at startup and read one frame —
fail loud on a missing/garbage file, per CLAUDE §4.6). The file's *native*
sample rate is recorded but **not** required to match any call — see §4.

`ParkConfig` (compiled) → mapped to core's `ParkLimits` (core doesn't dep
on config, same pattern as `ConferenceLimits`).

---

## 3. WS protocol additions (version stays `"1"` — additive)

- `BridgeIn::Park { call_id, slot: Option<String> }` — a WS server parks
  its **own** call (self-scoped, §9.2). `slot` is an optional human label
  for the lot.
- `StopReason::Park` — the `stop` SiphonAI sends before closing the WS on
  park. The server learns "you were parked, not hung up."
- `StartMsg.retrieved: bool` (`#[serde(default)]`, skip-if-false) — `true`
  on the `start` of a retrieve session so the new WS server knows it's
  picking up a parked call, not a fresh inbound one.

Retrieve has **no** WS-server-initiated message (operator-only). PROTOCOL.md
§3/§4 updated in the same PR.

---

## 4. MOH source (media-glue)

A small `MohSource` built **per park, at the call's negotiated rate**:

- `moh_file` set **and** its native rate == the call's rate → looping
  `FileSource` (`open_trusted`; on `EndOfFile`, `reset()` and continue —
  forge exposes `reset()` = seek-to-zero).
- otherwise (unset, rate mismatch — forge has no resampler — or open error)
  → `ToneGenerator::comfort_noise(rate)` (infinite, any rate). A rate
  mismatch logs once at `info` and falls back; it is **not** a park
  failure.

`MohSource::next_frame(samples)` returns one 20 ms PCM16 frame (`Vec<i16>`),
never erroring (the looping/fallback guarantees an infinite stream).

The startup `moh_file` check (decodability) catches gross misconfig early;
the per-park rate decision is where fallback actually happens.

---

## 5. Tap changes (media-glue) — Park / Unpark modes

The tap already multiplexes audio routing (direct pair ↔ room). Park adds a
third mode, driven by two new `TapCommand`s:

```rust
TapCommand::Park   { moh: MohSource }
TapCommand::Unpark { caller_audio_tx, playout_audio_rx, events_tx }
```

**On `Park`:** the tap stops using the WS-facing channels and switches to a
20 ms `tokio::time::interval` (monotonic, §4.3 — same cadence the room
uses) that pulls `moh.next_frame()` and pushes it to forge playout.
Inbound caller frames are **dropped** (no WS to forward to). DTMF/VAD/etc.
events are dropped too (no events sink). A `Park` while in a room first
leaves the room (drops the `RoomSender`).

**On `Unpark`:** the tap swaps its three WS-facing endpoints
(`caller_audio_tx`, `playout_audio_rx`, `events_tx`) to the fresh ones the
controller supplies (these become reassignable locals, like the room sink
receivers are today), stops the MOH tick, and resumes the direct pair.

The tap's `run()` already holds `playout_audio_rx` as `mut`; `caller_audio_tx`
and `events_tx` become `mut` locals so they can be swapped. **No new
allocations in steady state** — the MOH tick reuses the same per-frame
budget as the room playout arm.

`MediaTap` itself never ends on park — the tap task is the durable owner.

---

## 6. Controller lifecycle — the crux

Today `CallController::run()` fuses `bridge_task` + `tap_task`: either one
ending → `break` → teardown. Park makes **the tap durable and the bridge
swappable**, with the controller persisting across the whole park episode.

### 6.1 State

Add a `parked: bool` (false initially). The select loop gains: a guard on
the bridge-end arm, two new command sources (retrieve, park — see §6.4), a
timeout arm, and a relay-free channel swap on retrieve.

### 6.2 Park transition (from a `Park` request — WS or admin)

1. Build `MohSource` at the call's rate (§4); `tap_cmd_tx.send(Park{moh})`.
2. `control_out_tx.send(Stop { reason: Park })` → the current bridge sends
   `stop{park}` to the WS and closes → `bridge_task` completes.
3. Set `parked = true`. Register in `ParkRegistry` (§7). Bump
   `siphon_ai_parked_calls_active`, `parks_total{result="ok"}`. Fire
   `call_parked` webhook. Arm the timeout (§8).
4. The **bridge-end arm is guarded** `if !parked` — so when `bridge_task`
   completes for a park, the arm does **not** fire (no `break`). We capture
   the prior `bridge_result` when we send the park Stop, before flipping
   `parked`. (A completed `JoinHandle` must never be re-polled; the guard
   guarantees it isn't until retrieve reassigns it.)

### 6.3 Retrieve transition (admin only)

Retrieve hands the controller a target `ws_url` (defaulting to the call's
original bridge `ws_url`). The controller, **on its own task** (so the
multi-RTT WS connect never blocks anything):

1. Build **fresh** channels: `caller_audio` (tx→tap, rx→bridge),
   `playout` (tx→bridge, rx→tap), `control_in` (tx→bridge, rx→controller),
   `control_out` (tx→shared, rx→bridge).
2. Build a fresh `StartMsg` from the **preserved call facts** (the
   controller keeps a clone of the original `start` minus `seq`), with
   `retrieved = true` and the (possibly new) `ws_url`.
3. `bridge_task = tokio::spawn(connect_and_run(bridge_cfg, start, BridgeChannels{…}))`;
   re-enable the bridge-end arm.
4. Swap the controller's locals: `control_in_rx = ci_rx`,
   `control_out_tx = co_tx`.
5. `tap_cmd_tx.send(Unpark { caller_audio_tx, playout_audio_rx, events_tx: co_tx.clone() })`.
6. `parked = false`. Deregister from `ParkRegistry`, disarm timeout. Bump
   `retrieves_total{result="ok"}`, decrement `parked_calls_active`. Fire
   `call_retrieved` webhook.

### 6.4 Channel ownership — why this is clean

The **one** shared piece is the handle's `bridge_events_tx` (cloned to the
tap and used by the acceptor's `on_reinvite` for Hold/Resume). To let a
fresh bridge receive events from all producers after retrieve, make it an
`arc_swap::ArcSwap<mpsc::Sender<OutgoingEvent>>` on the `CallHandle` (arc-swap
is already a workspace dep — the SIGHUP cert path uses it). `push_bridge_event`
loads the current sender; retrieve stores the new one.

**Crucially, `control_out_rx` stays owned by the bridge** (not relayed
through the controller). So the existing teardown invariant is untouched:
`control_out_tx.send(Stop).await; break;` still delivers `Stop` straight to
the bridge. Retrieve only swaps *senders* (controller local + tap +
handle's ArcSwap) and gives the new bridge a fresh *receiver*. No relay, no
Stop-ordering hazard.

### 6.5 Teardown while parked

- **Caller BYE / CANCEL:** unchanged. The SIP layer calls
  `CallRegistry::terminate*` → `handle.shutdown()` → the controller's
  shutdown arm runs teardown. The `tap_task` also ends when forge RTP stops.
  Either path is terminal; the parked-state cleanup (deregister, gauge
  decrement, `parks_total` already counted) runs in the teardown tail.
- **`max_parked` cap:** a park beyond the cap is refused —
  `parks_total{result="rejected"}`, and (WS) an `error{code:"park_failed"}`;
  the call continues unparked.
- **Daemon shutdown:** the controller's existing shutdown path tears the
  parked call down like any other.

---

## 7. `ParkRegistry` (core)

`call_id → ParkedHandle`, same §4.4 shape as `ConferenceRegistry` /
`CallControlRegistry`: exact-id, insert on park, remove on retrieve/teardown,
no enumeration of call internals. `ParkedHandle` carries the optional
`slot`, the `parked_at` instant (for `GET /parked` age + CDR), and the
retrieve is driven through the existing `CallHandle` command channel (§6.3),
not a separate one — `ParkRegistry` just records *that* a call is parked and
its metadata for the admin list.

Retrieve flow: admin → `ParkRegistry.lookup(call_id)` confirms it's parked →
`CallControlRegistry.lookup(call_id)` → `handle.request_retrieve(ws_url)`
(new `CallHandle` method, mirrors `request_conference_join`).

Park via admin similarly: `handle.request_park(slot)`.

`max_parked` is enforced in `ParkRegistry` (live count) at park time.

---

## 8. Timeout (core)

On park, arm a deadline = `parked_at + timeout_secs` (skip if `0`). In the
controller's select loop, a pinned `tokio::time::Sleep` arm, active only
while `parked`:

- fires → `park_timeout` webhook, then `timeout_action`:
  - `hangup` → drive normal teardown (`handle.shutdown()` semantics).
  - `keep` → stay parked, disarm (no repeat); operator must retrieve/hangup.

**Races:** retrieve before timeout → `parked=false` disables the arm.
Caller BYE before timeout → teardown disables it. The timer lives in the
controller loop (single owner), so there's no cross-task race — the
`parked` flag gates the arm.

---

## 9. Recording, CDR, metrics, webhooks

- **Recording:** a recording in progress at park keeps writing — the tap
  fork is unchanged; the parked span records the MOH the caller hears
  (consistent with "what the caller heard"). No new recording control.
- **CDR (additive, schema stays v1):** `park: { count, total_ms }` —
  `count` = number of park episodes this call had, `total_ms` = cumulative
  parked wall-time. Omitted when the call was never parked.
- **Metrics:** `siphon_ai_parked_calls_active` (gauge),
  `siphon_ai_parks_total{result=ok|rejected}`,
  `siphon_ai_retrieves_total{result=ok|not_parked|failed}`.
- **Webhooks:** `call_parked { call_id, slot? }`,
  `call_retrieved { call_id, ws_url }`,
  `park_timeout { call_id, action }`.

---

## 10. Admin API (telemetry) — `ParkAdminHandle`

| Method | Path | Result |
|---|---|---|
| GET    | `/admin/v1/parked` | list parked calls (`call_id`, `slot`, `parked_secs`) |
| POST   | `/admin/v1/calls/:call_id/park` `{slot?}` | `202` (dispatched) / `404` unknown call / `503` `max_parked` / `501` disabled |
| POST   | `/admin/v1/calls/:call_id/retrieve` `{ws_url?}` | `202` / `404` unknown-or-not-parked / `501` disabled |

`park`/`retrieve` are **`202` (dispatched)** — same pattern as conference
add/remove: the admin signals the call via its `CallHandle`, the call's own
controller does the work (§4.4), and the outcome surfaces on the (old/new)
WS + webhooks. `GET /parked` reads `ParkRegistry`. Core impl `ParkAdmin`
wires `ParkRegistry` + `CallControlRegistry`, mirroring `ConferenceAdmin`.

---

## 11. Test plan

- **media-glue:** `MohSource` loops a short WAV fixture (rate match) and
  falls back to comfort noise on rate mismatch / unset; tap Park→MOH ticks
  into forge, Unpark swaps back to fresh WS channels (synthetic PCM).
- **core:** controller park→retrieve round-trip over a stub WS server
  (park yields `stop{park}` + WS close; the call stays alive; retrieve
  opens a fresh WS that receives `start{retrieved:true}`; ends normally).
  Timeout→hangup and timeout→keep. Caller-BYE-while-parked tears down
  cleanly. `ParkRegistry` + `ParkAdmin` unit tests (cap, not-parked, etc.).
- **bridge:** round-trip the new `park` / `stop{park}` / `start.retrieved`.
- **telemetry:** the three admin routes + status mapping.
- **config:** `[park]` load/validate (bad action, missing moh_file, cap 0).
- **SIPp (chunk 5):** park→retrieve→hangup and park→timeout→hangup.

---

## 12. Decisions to confirm

1. **`ArcSwap` on `CallHandle.bridge_events_tx`** (§6.4) — the one
   shared-state change, needed so a retrieved call's fresh bridge receives
   acceptor-pushed Hold/Resume events. arc-swap is already a dep.
   *Alternative:* accept that Hold/Resume is dropped on a retrieved call
   until a future cleanup (simpler, slightly degraded). **Recommend ArcSwap.**
2. **CDR `park.count` semantics** — episodes (a call can park/retrieve
   repeatedly). OK?
3. **Retrieve onto a different `ws_url`** is allowed (defaults to the call's
   original). Confirm operators may redirect the retrieved session
   elsewhere.
4. **Park applies to inbound *and* outbound calls** (any call in
   `CallControlRegistry`), consistent with chunk 3. OK?
5. Everything else follows the locked plan (§9.3).

---

## 13. Deviations from this design (as built)

Small departures from the draft above, recorded per the header. None
change the locked decisions (§12), all of which held — including the
`ArcSwap`-style bridge-sender swap (§6.4), realised as
`CallHandle::swap_bridge_sender` invoked on retrieve.

1. **`max_parked` is not a `503` at the admin API.** §10 sketched a
   `503` from `POST /…/park` at the cap. As built, the admin `park`
   route only confirms the call exists (`202`/`404`); the cap is
   enforced in the call's own controller (`ParkRegistry::try_park`) and
   a refusal surfaces as `error { code: "park_failed" }` on the call's
   WS plus `parks_total{result="rejected"}` — consistent with the
   "dispatch-and-return, outcome on the WS" model the rest of §10 uses.
2. **Retrieve of a live (non-parked) call returns `409`,** not the
   `404` §10 folded into "unknown-or-not-parked". Unknown call → `404`;
   known-but-not-parked → `409 Conflict` (`ParkAdminError::NotParked`),
   which distinguishes operator error from a bad id.
3. **`retrieves_total` has labels `ok | not_parked` only** (§9 listed a
   third `failed`). A retrieve either dispatches (`ok`) or no-ops on a
   non-parked call (`not_parked`); there is no synchronous failure mode
   left to count, so `failed` was dropped rather than left unemitted.
4. **CDR `park` is a nested object `{ count, total_ms }`** (cdr
   `ParkInfo`), matching §9's shape; `count` is park *episodes* per
   §12.2.
