# Conference Rooms

SiphonAI can mix **N calls into one room** (0.7.0). Every leg keeps its own
WebSocket session — there is no single "host" bot — and each side hears the
room minus its own input, so a caller never hears themselves and a bot never
hears its own playout, but a caller still hears their own bot (STT keeps
working). Rooms are driven by the WS server (for its own call) and by
operators (for any call) over the admin API.

```
caller A ──RTP──► SiphonAI ─┐                     ┌─► WS server A (bot A)
caller B ──RTP──► SiphonAI ─┤  one mixed room      ├─► WS server B (bot B)
caller C ──RTP──► SiphonAI ─┘  (mix-minus-self)    └─► WS server C (bot C)
```

A room of N member calls mixes **2N** streams — each call's SIP caller and
its WS session — and hands each side the mix minus its own contribution.

## 1. Enabling

Conferencing is **off by default** and fail-closed: a `conference_join` is
refused (`error { code: "conference_failed" }`) unless `[conference]` is
turned on. See `docs/CONFIG.md` (`[conference]`) for every field; the short
version:

```toml
[conference]
enabled                   = true   # false (default) = every join refused
max_rooms                 = 16     # live rooms across the daemon (≥ 1)
max_participants_per_room = 8       # member CALLS per room (≥ 2)
join_tones                = false  # short chime into the room on join/leave
```

A 0.6.x config upgrades with zero behaviour change. Conferencing is a
daemon-level facility — global only, no per-route overrides.

## 2. Joining a room (WS server, self-scoped)

A WS server puts **its own** call into or out of a room over the protocol
(`docs/PROTOCOL.md` §4.8):

```json
{ "type": "conference_join",  "call_id": "...", "room_id": "support-7" }
{ "type": "conference_leave", "call_id": "..." }
```

- **`conference_join`** creates the room if it doesn't exist yet (subject to
  `max_rooms` / `max_participants_per_room`). On success SiphonAI replies
  `conference_joined { room_id, participants }` and the call's audio is mixed
  into the room. Joining a second room moves the call (it leaves the first).
- **`conference_leave`** removes the call from its room and restores the
  direct caller↔WS pair; SiphonAI replies `conference_left { reason: "left" }`.

A bot tracks the rest of the room via the fan-out events
`participant_joined` / `participant_left { room_id, participant_call_id }`
(`docs/PROTOCOL.md` §3.12), sent to every *other* member when the room's
composition changes.

**Self-scoped (§9.2):** a WS message acts only on the session's *own* call. A
bot can put itself in or out of a room, but it cannot add or remove *another*
participant — that's the operator control plane's job ([§3](#3-operator-control-admin-api)).

## 3. Operator control (admin API)

Operators compose and inspect rooms over the admin HTTP API
(`docs/DEPLOY.md`). Requires `[conference].enabled = true` (all routes `501`
otherwise).

These routes live on the dedicated **`[admin]` listener** (0.10.0), gated by
a bearer token + RBAC — they are *not* on `[observability].http_listen`,
which returns `404` for `/admin/*`. Listing a room needs **`readonly`**;
create / end / add / remove need **`operator`**. No token → `401`, too low a
role → `403`. Configure the listener and tokens per `docs/DEPLOY.md` →
Admin auth & RBAC, and keep it on loopback or a private interface
(`[admin.tls]` if it must bind somewhere routable).

```sh
ADMIN=http://127.0.0.1:9092          # https://… when [admin.tls] is set

# Who's in which room (readonly)
curl -s -H "Authorization: Bearer $SIPHON_ADMIN_RO" $ADMIN/admin/v1/conferences
# → {"count":1,"conferences":[{"room_id":"support-7","sample_rate":8000,
#     "participants":["siphon-a","siphon-b"]}]}

# Pull any active call (inbound or outbound) into a room — creates it if absent
curl -X POST $ADMIN/admin/v1/conferences/support-7/participants \
    -H "Authorization: Bearer $SIPHON_ADMIN_OP" \
    -d '{"call_id":"siphon-c"}'        # → 202

# Drop one call back to its private bot
curl -X DELETE -H "Authorization: Bearer $SIPHON_ADMIN_OP" \
    $ADMIN/admin/v1/conferences/support-7/participants/siphon-c   # → 202

# End the whole room (every member reverts to its direct pair)
curl -X DELETE -H "Authorization: Bearer $SIPHON_ADMIN_OP" \
    $ADMIN/admin/v1/conferences/support-7   # → 200
```

`add`/`remove` return **`202` (dispatched)**: the daemon signals the target
call, which joins/leaves on its **own** WS session — the outcome surfaces
there (`conference_joined` / `conference_left` / `error`), not in the HTTP
response. This respects CLAUDE.md §4.4 (no reaching into another call's
controller): the admin resolves the target through a daemon-wide bridge-id →
`CallHandle` registry and pushes a command the call's own controller runs.
`create` returns `201 {room_id}` (a generated id when the body omits one);
`409` if the id is already live; `503` at the `max_rooms` cap; `400` for a
`sample_rate` other than 8000/16000.

## 4. What each side hears

The room model is **mix-minus-self**. For N member calls the room mixes 2N
streams (each call's SIP caller + its bot) and gives each side the mix minus
its own input:

- a **caller** hears every other caller and every bot, but not themselves;
- a **bot** hears every caller (including its own — so its STT still works)
  and every other bot, but not its own playout.

Leaving a room, or the room dying (last member left / operator force-end),
always restores the direct caller↔WS pair. Per-leg recording keeps working
while in a room — the recorded "bot" channel is the room mix the caller
actually heard.

## 5. Observability

- **Metrics** — `siphon_ai_conferences_active` (live rooms),
  `siphon_ai_conference_participants` (mixer participants across all rooms;
  each member call contributes 2), `siphon_ai_conference_joins_total{result}`
  (`joined` / `disabled` / `too_many_rooms` / `room_full` / `rate_mismatch` /
  `already_joined` / `error`), plus the room-health gauges
  `siphon_ai_room_tick_lag_seconds` and
  `siphon_ai_room_frames_dropped_total{stage,side}`. See `docs/DEPLOY.md`.
- **Webhooks** — `conference_created` (first join / admin pre-create) and
  `conference_ended { duration_ms, peak_participants }` (last leave /
  force-end), pairing 1:1. Payloads in `docs/DEPLOY.md`.
- **Logs** — join/leave/room-lifecycle at `info`/`debug` with `call_id` and
  `room_id` in the span.

## 6. Limitations (0.7.0)

- **One sample rate per room.** A room locks to its first joiner's negotiated
  rate (8 kHz or 16 kHz); a join at a different rate is rejected
  (`rate_mismatch`) — there is no resampling in 0.7.0.
- **`max_participants_per_room` is kept small (default 8).** Per-sink
  mix-minus-self is O(N²); the cap bounds per-tick mixing cost.
- **No DTMF-IVR / PIN / host controls.** SiphonAI uses `forge-mixer` +
  `forge-injection` directly, not `forge-conference` — menus, PINs, and
  moderator controls are a WS-server concern (DEV_PLAN_0.7.0.md §9.4).
- **SIPp coverage asserts lifecycle, not mixed audio.** The two-caller SIPp
  phase verifies that two calls land in one room (metrics) and tear down
  cleanly; audio-correctness is covered by media-glue unit tests with
  synthetic PCM.
