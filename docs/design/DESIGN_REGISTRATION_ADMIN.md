# Design note — registration management (admin API)

> **Status: APPROVED — decisions LOCKED (§9, all per recommendation,
> 2026-07-15).** Pins the admin
> surface, the task-wiring mechanics, and the failure semantics before
> implementation — the usual design-first pass. The build follows this;
> deviations get noted back here. Target: **0.33.0**. No WS-protocol,
> CDR, or webhook-schema changes; no new config.

Gives operators **write actions on `[[register]]` bindings** without
bouncing the daemon. Today a `[[register]]` row re-REGISTERs only on
its own refresh timer (`expires − margin`) or on the failure backoff
(5 s → 300 s cap); when an upstream (e.g. CUCM) drops or stales a
binding server-side, the only recovery is a daemon restart — which
tears down every active call. This theme extends the existing
read-only `GET /admin/registrations` (0.10.0) with two per-binding
write actions on the authenticated `[admin]` listener, neither of
which touches media:

| Action | What it does | When you reach for it |
|---|---|---|
| **`refresh`** | Fire an immediate authenticated REGISTER, off-cycle | Binding looks stale; registrar restarted; "just re-assert it now" |
| **`restart`** | Full cycle: REGISTER `Expires: 0` (clear the binding), then a fresh REGISTER | Server-side stale state a refresh can't fix; contact rebinding after an IP change |

---

## 1. Admin surface

New endpoints (versioned prefix, matching every post-0.10.0 addition;
the legacy unversioned `GET /admin/registrations` stays where it is):

- **`POST /admin/v1/registrations/{name}/refresh`**
- **`POST /admin/v1/registrations/{name}/restart`**

Both are empty-body POSTs. Responses:

- **`202 Accepted`** — the command was queued to the binding's drive
  task. Body: `{ "accepted": true, "action": "refresh", "registration":
  <row> }` where `<row>` is the same shape as one `GET
  /admin/registrations` entry (the state *at accept time* — see §3 on
  why the outcome is asynchronous).
- **`404`** — no `[[register]]` block with that `name`.
- **`409 Conflict`** — the daemon is draining (shutdown in progress);
  the drive tasks are already winding down.

**Role: `operator`** (live-state control, not billable/config — the
same tier as park/retrieve/hangup). Audit-logged automatically like
every admin request (actor = token name, 0.20.0 stream included);
counted in `siphon_ai_admin_requests_total` via a new bounded
`endpoint` template label. **Per-binding only** — no global
"refresh all" in v1; operators can script the list from the `GET`.

No new config: the endpoints exist whenever `[admin]` is served, which
is already opt-in with a secure default (omit `[admin]` → no `/admin`).

---

## 2. Task wiring — nudge the existing loop, spawn nothing

Each `[[register]]` block already runs one drive task
(`bins/siphon-ai/src/registration.rs`) whose waits — the
refresh-timer sleep and the failure backoff — are a 2-arm
`interruptible_sleep(delay, shutdown)`. The mechanics are exactly the
ROADMAP sketch: add a **third wake source** instead of spawning
anything.

```rust
enum RegistrationCommand { Refresh, Restart }

// per-binding, created at spawn time:
let (cmd_tx, cmd_rx) = mpsc::channel::<RegistrationCommand>(2);
```

- The drive loop's sleeps become `select!` over **timer / command /
  shutdown**. A command during the *registered* sleep fires the action
  immediately; a command during the *failure backoff* short-circuits
  the backoff (an operator kick is also "retry now", which resets the
  backoff to initial — an explicit human action outranks the
  exponential politeness timer).
- A command that arrives **while a REGISTER round-trip is already in
  flight** just sits in the channel (bound 2) and is consumed at the
  next loop turn — no interleaved REGISTERs for one binding, ever.
  A full channel (operator hammering the button) drops the extra
  command at the endpoint with `202` all the same: the queued one
  already guarantees a fresh cycle. (Coalescing note: `refresh` behind
  a queued `restart` is subsumed; `restart` behind a queued `refresh`
  is not — bound 2 keeps both.)
- **`restart`** = `uac.register(registrar, Some(0))` (clear the
  binding, RFC 3261 §10.2.2) then the normal register step.
  A failed unregister is logged at `warn` and the fresh REGISTER
  proceeds anyway — the goal state is "registered", and a registrar
  that errored the `Expires: 0` will still replace the binding on the
  follow-up REGISTER. Only the *final* REGISTER's outcome drives
  status/metrics/webhook, so a restart never leaves the row parked on
  a cosmetic unregister failure.

**Command registry.** `RegistrationManager` (sip-glue) grows a
per-name `cmd_tx` map populated at spawn time. The runtime installs a
small `RegistrationAdminHandle` trait object into `AdminState` —
exactly the `ParkAdminHandle` / `OutboundOriginateHandle` pattern —
so `telemetry` gains no dependency on sip-glue. Registrations are
restart-required config (not SIGHUP-reloadable), so the map is static
for the process lifetime: no `ArcSwap`, no reload interaction.

---

## 3. Why `202` and not "wait for the outcome"

A REGISTER round-trip is unbounded in practice (Timer F is 32 s; the
UAC also drives 401/407 retries internally). Holding the admin HTTP
request for that would (a) invite client timeouts that desync the
operator's view, and (b) break the precedent that admin write actions
are fire-and-forget signals (`park`, `retrieve`, originate all return
`202`). The outcome is already fully observable out-of-band, today:

- `GET /admin/registrations` — status flips `pending/registered/failed`
  with `expires_at` / `last_error`;
- `siphon_ai_register_attempts_total{name,outcome}` ticks on the
  attempt; `siphon_ai_register_state{name,state}` flips;
- the `registration_state_changed` lifecycle webhook fires on every
  transition.

The `202` body carries the accept-time row so a CLI one-liner can show
"was `failed`, kicked" without a second call. *(Decision §9.2 offers
the bounded-wait alternative for the record.)*

---

## 4. Disabled bindings — the reserved "tell to register" RPC

`RegistrationStatus::Disabled` (`register_on_startup = false`) today
spawns a no-op task, and the enum's doc comment explicitly reserves
the admin follow-up: *"v1 has no 'tell to register' RPC; this is
reserved for the admin-endpoint follow-up."* This is that follow-up.

**Proposal: unify instead of special-casing.** A disabled block runs
the *same* drive task, parked in a "wait for first command" state
before its first REGISTER. `refresh` (or `restart` — identical from
the parked state) starts the normal cycle; from then on the binding
refreshes itself like any other. The no-op `spawn_disabled_task` is
deleted; `Disabled` remains the row's status until the first
operator-triggered attempt flips it to `pending` → whatever the
attempt yields.

This makes `register_on_startup = false` mean something useful —
"registered under operator control" (maintenance windows, staged
cutovers) — with zero new machinery beyond the command channel the
theme adds anyway. One deliberate gap: there is **no "stop
registering / re-disable" action** in v1 (that's an unregister +
park, a different lifecycle) — noted in §8 as a future follow-up.

---

## 5. Observability (same PR as the feature)

- **Metric** — `siphon_ai_register_admin_triggers_total{name, action}`
  (both labels bounded: operator-chosen `[[register]]` names ×
  `refresh|restart`). Counts *accepted* triggers; the resulting
  REGISTER outcome lands on the existing `register_attempts_total`.
  Documented in `docs/DEPLOY.md`.
- **`siphon_ai_admin_requests_total`** — the two new endpoint
  templates added to the bounded `endpoint` label mapper
  (`auth.rs::endpoint_label` + `min_role` parameterised arms).
- **Logs** — `info!` on command accept (name, action, actor comes from
  the admin request log) and inside the drive loop on command receipt;
  the existing per-transition logging covers the rest.
- **Audit** — automatic via the 0.10.0/0.20.0 `admin_request` event
  (method + path + actor + outcome); no new audit event type.
- **Webhook** — no new event; `registration_state_changed` already
  reports every resulting transition.

---

## 6. Testing

- **Unit (sip-glue)** — command-registry lookup (known/unknown name),
  channel-full behavior, manager snapshot unchanged by queued
  commands.
- **Unit (bins drive loop)** — the select-arm mechanics: command
  during registered-sleep triggers an immediate attempt; command
  during backoff resets it; restart performs expires-0 then register;
  parked-disabled task starts on first command. (The loop's UAC calls
  are already isolated behind `perform_register`; a test seam there —
  trait or closure — keeps these tests free of live SIP.)
- **Unit (telemetry)** — endpoint dispatch with a mock
  `RegistrationAdminHandle` (the `ParkAdminHandle` test pattern):
  202/404/409 shapes, role gating (operator required, readonly
  forbidden), endpoint label bounded.
- **SIPp e2e** — new auxiliary phase: SIPp plays a *registrar UAS*
  (answers REGISTER with 200 + `Expires`), the daemon registers with a
  long `expires`, the phase then POSTs `refresh` and asserts a second
  REGISTER arrives well before the refresh timer could have fired;
  then POSTs `restart` and asserts the `Expires: 0` + re-REGISTER
  pair. First registrar-side SIPp scenario — also closes a standing
  e2e gap for `[[register]]` mode generally.

---

## 7. Docs

- `docs/DEPLOY.md` — the two endpoints in the admin API table +
  the new metric row.
- `docs/REGISTRATION.md` — an "operating registrations" section:
  when to refresh vs restart, the disabled-until-kicked pattern,
  and the observable outcome trail (§3 list).
- `docs/ROADMAP.md` — mark the P2 item delivered (same PR as release).

---

## 8. Explicitly out of scope (v1 of this theme)

- **Global "refresh all"** — script the `GET` list; revisit on demand.
- **"Unregister / re-disable" action** — a different lifecycle
  (deliberate deregistration + parking the task); needs its own
  status semantics. Natural follow-up if maintenance-window demand
  shows up.
- **DNS failover / SRV re-resolution on restart** — v1 registrar
  addressing stays as-is (resolved `host:port` from config).
- **SIGHUP-reloadable `[[register]]` blocks** — separate theme;
  registrations stay restart-required config.

---

## 9. Decisions to lock

1. **Paths: `POST /admin/v1/registrations/{name}/refresh|restart`**,
   legacy `GET /admin/registrations` unchanged. *(Alternative: also
   add a versioned `GET /admin/v1/registrations` alias for symmetry.)*
   **Recommend the two POSTs only** — an alias adds a second identical
   read surface to keep documented forever, for zero capability.
2. **`202` fire-and-forget** with the accept-time row in the body;
   outcome via GET/metrics/webhook (§3). *(Alternative: hold the
   request up to a bounded wait, e.g. 5 s, then return the resulting
   state.)* **Recommend `202`** — matches every existing admin write
   and dodges the 32 s worst case.
3. **Operator role** for both actions. **Recommend as stated** — it's
   live-state control, same tier as park/retrieve.
4. **Refresh during backoff resets the backoff** to initial (operator
   kick = "retry now with a clean slate"). **Recommend yes.**
5. **Restart proceeds past a failed unregister** (warn + continue to
   the fresh REGISTER; only the final attempt drives status).
   **Recommend yes** — the goal state is "registered", not "cleanly
   unregistered".
6. **Disabled bindings join the theme** (§4): the parked-drive
   unification, `refresh`/`restart` both start a parked binding, no
   re-disable action in v1. **Recommend yes** — it deletes the no-op
   task, fulfills the reserved TODO, and costs only the machinery the
   theme already builds. *(Alternative: `409` on disabled bindings and
   keep the no-op task.)*
7. **New counter `siphon_ai_register_admin_triggers_total{name,action}`**
   (vs relying on `admin_requests_total` alone). **Recommend yes** —
   the generic counter can't answer "which binding is being kicked and
   how often", which is the fleet-health question.
8. **One release (0.33.0), two PRs** — feature (everything in §1–§7)
   then release. The theme is small enough not to chunk further.
   **Recommend as stated.**
