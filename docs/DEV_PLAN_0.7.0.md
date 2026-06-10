# SiphonAI 0.7.0 Development Plan

> **Status: decisions LOCKED** (user, 2026-06-10): conference model = **every
> leg keeps its WS session** (§9.1); control surface = **full WS message set
> AND full admin CRUD** (§9.2); park = **media-only** (no SIP re-INVITE hold,
> §9.3); **no built-in IVR** — the bot drives (§9.4). WS protocol stays
> `version: "1"` (all additions are new messages / optional fields). Ready to
> execute chunk-by-chunk.

Theme: **conferencing + park** — the first multi-party theme. Until now every
call is a closed pair (one SIP leg ↔ one WS session); 0.7.0 adds two ways to
recompose that pair without ending the call:

- **Conference**: N SIP legs share one mixed audio room. Each leg's WS
  session stays connected: each bot hears the room (minus its own playout)
  and speaks into it. The room is how a bot keeps the caller, the consulted
  human, and itself in one conversation — the supervision/3-way upgrade of
  0.6.1's attended handoff (which removes the bot from the call).
- **Park**: a call is shelved without a WS session — the caller hears hold
  music (or comfort noise), the SIP dialog stays up, and the call is later
  retrieved onto a fresh WS session, transferred, joined to a room, or hung
  up. Park is the "put them on ice" primitive between conversations.

```
            ┌────────────── conference room "support-7" ──────────────┐
caller A ↔ SiphonAI ↔ WS A (bot a)        mixer: each sink hears      │
agent  C ↔ SiphonAI ↔ WS C (bot c)        mix-minus-self @ 20 ms      │
            └──────────────────────────────────────────────────────────┘

caller B ↔ SiphonAI ─(parked: MOH loop, no WS)─ … later: retrieve → WS B'
```

This deliberately retires the "Conferencing or mixing" entry from CLAUDE.md
§8 / DEV_PLAN.md §8 (a v1 non-goal, now scheduled); the docs chunk updates
both (and fixes the stale "outbound" line 0.6.0 left behind).

## 0. Why this is buildable now (grounded, not assumed)

Upstream survey against the pinned forge-media (`e95a31a959a6`) and local
checkouts, 2026-06-10. **All three media crates build cleanly at the pin**
(`cargo check -p forge-mixer -p forge-injection -p forge-conference` — the
forge-recorder dependabot breakage noted in our Cargo.toml does not bite
these crates).

- **`forge-mixer` EXISTS and is sufficient** — `AudioMixer`
  (`forge-mixer/src/mixer.rs:240`): `add_participant(id, gain)` /
  `remove_participant`, push input via `write_samples(&[i16])`, pull output
  via `mix()` / **`mix_excluding(exclude_id)`** (the mix-minus-self we need
  per sink), additive mixing with clamp + auto-gain (`1/√n`), per-participant
  `set_gain` and `set_participant_state(Active|Muted|OnHold)`, built-in RMS
  VAD (`is_speaking`). Format-locked per mixer (one sample rate); buffering
  is per-participant `VecDeque` with a frame-size threshold.
- **`forge-injection` EXISTS** — `FileSource` (symphonia: WAV/MP3/OGG/…),
  `ToneGenerator::{comfort_noise, single_tone, silence}`, `MixMode::{Mix,
  Replace, Duck}`, and `InjectionSandbox` (path-jailed file access) — the
  MOH/park audio source and the join-tone source.
- **`forge-conference` exists but we will NOT depend on it** — its
  `ConferenceRoom` bundles DTMF IVR menus (*6/*9/host PINs), PIN auth,
  recorder wiring, and an AI-manager (forge-engine `AISessionManager`).
  Decision §9.4 says the bot drives; depending on `forge-mixer` +
  `forge-injection` directly keeps the dep tree minimal and the control
  plane in the WS server. (No feature gates on any of the three; none pull
  `forge-ai-stream`.)
- **No new SIP machinery needed.** Park is media-only (§9.3): the dialog
  stays sendrecv on the wire and we keep sending RTP (which MOH requires
  anyway). siphon-rs *does* expose locally-initiated re-INVITE
  (`IntegratedUAC::send_reinvite`, `sip-uac/src/integrated.rs:2053`) if a
  later release wants standards-pretty hold; explicitly out of scope now.
- **SiphonAI-side attach points verified** — the per-call tap
  (`media-glue/src/tap.rs`) already runs a 20 ms reframed PCM16 loop with
  command channel (`TapCommand`), playout flush, and a recording fork added
  in 0.5.0 — the conference redirection and MOH source slot in at the same
  layer. Registries precedent: `CallRegistry` / `ConsultRegistry`
  (`core/src/registry.rs`) show the CLAUDE §4.4-compliant shape for
  daemon-wide, exact-id, no-enumeration lookup structures.

**No upstream work required for must-have scope.** One known wart: the
mixer's `mix*()` APIs return a fresh `Vec<i16>` per call — see §7 risks.

## 1. Already shipped (context this builds on)

- 0.6.0 outbound origination: consult/extra legs are ordinary outbound calls
  (`POST /admin/v1/calls`) — rooms are composed from calls that already
  exist; this plan adds **zero** call-creation surface.
- 0.6.1 attended transfer: the bot-steps-out handoff. Conferencing is the
  bot-stays-in alternative; the two share the "compose calls by call_id"
  mental model.
- 0.5.0 recording: per-leg stereo WAV (caller | what-the-leg-hears) keeps
  working unchanged in a room — the tap still sees both directions.
- Hold/resume (peer-initiated re-INVITE) events keep flowing per-leg.

## 2. Scope (must-have)

### 2.1 Conference rooms (core + media-glue)

A **room** is a daemon-level task owning one `AudioMixer` and a 20 ms
`tokio::time::interval` tick (monotonic — CLAUDE §4.3; never a self-
correcting sleep). Participants are **pairs**: each joined call contributes
its SIP leg (caller audio in / mix-minus-self out) and its WS session (bot
audio in / mix-minus-self out) as two mixer participants. All plumbing is
bounded channels; the room task never touches another call's state — a
`ConferenceRegistry` (CallRegistry-style: exact-id, insert/remove, no
enumeration of call internals) maps `room_id → RoomHandle` (channel
senders + a control sender). CLAUDE §4.4 stance: a room is an *explicit
rendezvous point a call opts into*, identical in spirit to ConsultRegistry —
calls hand frames to the room; nothing reaches into a `CallController`.

- Join re-plumbs the tap via `TapCommand::JoinRoom{…}` /`LeaveRoom`: caller
  frames are forwarded to the room instead of the WS; the WS recv path is
  fed from the room's per-sink output; WS playout goes to the room instead
  of directly to RTP; RTP out is fed mix-minus-self. Leaving (or the room
  dying) restores the direct pair — a call ending mid-room just removes its
  two participants.
- **Sample-rate policy:** a room locks to the first joiner's negotiated rate
  (8 k or 16 k); a join at a different rate is **rejected** with a protocol
  error (no resampling in 0.7.0 — documented limitation).
- Caps: `[conference] max_rooms`, `max_participants_per_room` (calls, not
  mixer entries). Join beyond cap → error, call continues unchanged.
- Optional join/leave tones (`ToneGenerator`, config toggle, default off).
- DTMF events, VAD/speech events, silence/dead-air detection, recording,
  and `mute`/`unmute` (self-scoped) all keep working per-leg.

### 2.2 WS protocol surface (additive; version stays "1")

`BridgeIn` (all **self-scoped** — a session only acts on its own call;
cross-participant control lives on the admin surface, §2.3):

```jsonc
{ "type": "conference_join",  "call_id": "...", "room_id": "support-7" }
{ "type": "conference_leave", "call_id": "..." }
{ "type": "park",             "call_id": "...", "slot": "lot-3" }   // slot optional
```

`conference_join` creates the room if absent (subject to caps). `BridgeOut`:
`conference_joined{room_id, participants}`, `conference_left{room_id,
reason}`, `participant_joined{room_id, call_id}` / `participant_left{…}`
(fan-out to every session in the room), and errors via the existing
`error{code}` channel with new codes `conference_failed` / `park_failed`.
`park` ends the session with `stop{reason:"park"}` (new additive
`StopReason::Park`). Every new message/field/code is documented in
PROTOCOL.md in the same PR that adds it, and the example WS servers learn
to at least log the new events.

### 2.3 Admin API (full CRUD, §9.2)

Conference: `GET /admin/v1/conferences` (list + participant detail),
`POST /admin/v1/conferences` (pre-create, optional `room_id`),
`POST /admin/v1/conferences/:id/participants {call_id}` (add **any** active
call — bridged or parked), `DELETE …/participants/:call_id` (remove → the
leg reverts to its pair, or to parked if it came from park),
`DELETE /admin/v1/conferences/:id` (end room; legs revert). Park:
`GET /admin/v1/parked`, `POST /admin/v1/calls/:call_id/park`,
`POST /admin/v1/calls/:call_id/retrieve {ws_url?}` (defaults to the call's
route/bridge `ws_url`). Same no-native-auth reverse-proxy posture as the
originate API (0.6.0 §9.5) — these verbs move live calls; the admin bind
stays private. Body cap and dispatch follow the `admin.rs` originate
precedent.

### 2.4 Park (media-only, §9.3)

- `park` (WS or admin): detach the WS session (`stop{reason:"park"}`, WS
  closes), switch the tap's playout source to MOH — `[park].moh_file` via
  `InjectionSandbox`+`FileSource` (looped), falling back to
  `ToneGenerator::comfort_noise` (or silence) when unset. Caller audio is
  discarded while parked (BYE/CANCEL handling unchanged — a parked caller
  hanging up tears down normally).
- `ParkRegistry` (same §4.4-compliant shape) maps `call_id → ParkedHandle`,
  with optional named `slot` for human-facing lots.
- **Retrieve** attaches a *fresh* WS session to the live call (`start`
  carries a new `reason`-less session; `start.retrieved: true` additive
  field). This is deliberately **not** mid-call WS reconnect (still out of
  scope): retrieval is operator-initiated, on a parked call only, with a
  clean new session — no seq continuity, no replay.
- Timeout: `[park].timeout_secs` (default 300) → `park_timeout` webhook +
  configurable `timeout_action = "hangup" | "keep"` (default `"hangup"`).

### 2.5 Config (TOML, validated at load)

```toml
[conference]
enabled = false                  # fail-closed, like [outbound]
max_rooms = 16
max_participants_per_room = 8
join_tones = false

[park]
enabled = false
moh_file = "/etc/siphon-ai/moh.wav"   # optional; comfort noise if unset
timeout_secs = 300
timeout_action = "hangup"             # "hangup" | "keep"
max_parked = 32
```

Global only in 0.7.0 (no per-route overrides — rooms and lots are
daemon-level facilities); `moh_file` existence + decodability checked at
startup. Both features **off by default**: a 0.6.1 deployment upgrades with
zero behaviour change.

### 2.6 Observability (same PR as each feature)

- **Metrics**: `siphon_ai_conferences_active`,
  `siphon_ai_conference_participants` (gauge),
  `siphon_ai_conference_joins_total{result}`,
  `siphon_ai_room_tick_lag_seconds` (histogram — mixer cadence health),
  `siphon_ai_parked_calls_active`, `siphon_ai_parks_total{result}`,
  `siphon_ai_retrieves_total{result}`. Bounded labels only.
- **Webhooks**: `conference_created`, `conference_ended{duration_ms,
  peak_participants}`, `call_parked`, `call_retrieved`, `park_timeout`.
- **CDR** (additive, version stays 1): `conference: { rooms: [room_id…],
  total_ms }` and `park: { count, total_ms }`, omitted when empty.
- **Logs**: join/leave/park/retrieve at `info` with `call_id` + `room_id`
  span fields; room lifecycle at `info`.
- **HEP**: no new SIP messages (park is media-only), so SIP-side emission is
  unchanged; add an application event chunk for join/leave/park/retrieve so
  Homer's per-call timeline shows the composition changes.

## 3. Out of scope (deliberate, 0.7.x or later)

- SIP-level hold for park (locally-initiated re-INVITE sendonly) — siphon-rs
  `send_reinvite` exists; revisit if operators need the peer to *see* hold.
- Room resampling (mixed 8 k/16 k rooms) — rejected join for now.
- forge-conference's IVR (DTMF menus, PINs, host controls, kick/lock via
  DTMF) — rejected in §9.4; the bot/admin API are the control plane.
- Conference recording as a *room-level* artifact (per-leg recording already
  captures each perspective); revisit with the recording theme.
- Video, WebRTC, mid-call WS reconnect (retrieve ≠ reconnect, §2.4),
  per-route conference/park config.
- Parked-call music *per slot* / announcement scheduling.

## 4. Chunk plan (proposed)

1. **Room core** (core, media-glue, config): `[conference]` config,
   `ConferenceRegistry` + room task (mixer, 20 ms tick, channel fan-out),
   `TapCommand::JoinRoom/LeaveRoom` re-plumbing, join/leave by internal API
   only. Unit tests with synthetic PCM (two legs hear each other,
   mix-minus-self, rate-mismatch rejection, room teardown reverts legs).
   New deps: `forge-mixer`, `forge-injection` (workspace-pinned, same rev).
2. **WS protocol surface** (bridge, core): `conference_join`/`leave` +
   events + error codes + PROTOCOL.md; metrics; echo-server knob
   (`--auto-conference-join ROOM`) for the harness.
3. **Admin conference CRUD** (telemetry, core): the §2.3 conference
   endpoints incl. cross-call add/remove; webhooks `conference_*`;
   DEPLOY.md.
4. **Park + retrieve** (core, media-glue, bridge, telemetry): `[park]`
   config, MOH injection source, `park` WS message + `StopReason::Park` +
   `start.retrieved`, `ParkRegistry`, timeout task, park/retrieve/list
   admin endpoints, `park_*`/`call_parked` webhooks, CDR fields, metrics.
5. **Docs + SIPp + release**: `docs/CONFERENCE.md` (rooms + park guide),
   PROTOCOL.md polish, CLAUDE.md §8 + DEV_PLAN.md §3.3/§8 drift fixes,
   SIPp scenarios — (a) two SIPp callers join one room via auto-join knob;
   assert both legs stay up through join/leave and
   `conference_participants` reads 4 (2 legs × SIP+WS) then 0; (b) park →
   retrieve → hangup with `parks_total`/`retrieves_total` assertions;
   (c) park → timeout → hangup. CHANGELOG, version 0.6.1 → 0.7.0, tag.

Each chunk: branch → PR → CI green → squash-merge, per CLAUDE.md.

## 5. Definition of Done — v0.7.0

- Two live SIPp calls join one room and both far ends run to clean
  completion with the room's metrics/webhooks/CDR fields correct; leaving
  reverts each leg to its private WS pair (audio resumes pair-wise).
- A parked call plays MOH on the wire (harness: RTP keeps flowing while no
  WS session exists), then is retrieved onto a fresh WS session and ends
  normally; timeout path fires the webhook and the configured action.
- An admin operator can list rooms/parked calls and force-end both.
- A 0.6.1 config boots 0.7.0 with byte-identical behaviour (both features
  off by default); all existing SIPp scenarios pass unchanged.
- Protocol stays `version: "1"`; every new message/field/error code is in
  PROTOCOL.md; example servers handle (at least log) the new events.
- No allocations added to the per-frame paths *we own*; the mixer's
  internal allocation is measured (tick-lag histogram) and noted.

## 6. Risks

- **Mixer hot-path allocations** — `AudioMixer::mix*()` allocates a
  `Vec<i16>` per sink per 20 ms tick (2N allocs/tick). At the configured
  caps (8 participants) this is small, but it violates the spirit of CLAUDE
  §4.3 inside upstream code. Mitigation: measure via
  `room_tick_lag_seconds`; if it shows, a buffer-reuse `mix_into(&mut
  [i16])` API is a small forge-media PR (ask user first, per CLAUDE §2).
- **O(N²) mix-minus-self** — `mix_excluding` per sink re-sums the room.
  Fine at N≤8; the cap is the guardrail. Revisit (sum-once-subtract-self)
  upstream if caps ever grow.
- **Tap re-plumbing complexity** — join/leave swaps live audio routes on a
  running call. Mitigation: the swap happens in the tap task itself on a
  `TapCommand` (single owner, no locks), mirroring how mute/flush already
  work; leave/death always restores the direct pair (tested).
- **Retrieve ≈ reconnect confusion** — operators may expect mid-call WS
  reconnect generally. PROTOCOL.md/CONFERENCE.md state plainly: retrieval
  is a new session on a parked call, nothing else.
- **MOH file licensing/ops** — shipping no default music; comfort-noise
  fallback keeps the wire alive without bundling audio assets.

## 7. Decisions (LOCKED 2026-06-10, via AskUserQuestion)

- **§9.1 Conference model = every leg keeps its WS session.** Each joined
  call's bot stays connected, hears the room mix minus its own playout, and
  can inject audio. (Rejected: single "host" bot seat; SIP-only rooms.)
  Consequence: a room mixes 2N participants (N SIP + N WS) and all
  conference events fan out to every member session.
- **§9.2 Control surface = both, fully.** The complete WS message set
  (self-scoped) AND full admin CRUD (including cross-call participant
  add/remove) ship in 0.7.0. WS messages stay scoped to the session's own
  call; anything that touches *another* call goes through the admin API —
  preserving the per-call protocol scoping while giving operators full
  composition control.
- **§9.3 Park = media-only.** No re-INVITE hold in 0.7.0; the dialog stays
  sendrecv and SiphonAI keeps sending RTP (MOH needs that anyway). The
  upstream `send_reinvite` path is noted for a future release.
- **§9.4 No built-in IVR — the bot drives.** Depend on `forge-mixer` +
  `forge-injection` directly; do not depend on `forge-conference` (its DTMF
  menus/PIN auth/host controls stay unused). DTMF keeps flowing to each
  leg's WS server; mute/kick/lock are bot (self-scoped) or admin
  (cross-call) operations.
