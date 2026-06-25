# Design: graceful shutdown & connection draining

Status: **DECISIONS LOCKED (2026-06-24) — ready to implement** (forks locked
in §4; now chunked PRs, same cadence as config-CLI → v0.12.0 and
release-packaging → v0.16.0).

Theme: **P0 from `docs/ROADMAP.md`** ("Production operability → Graceful
shutdown & zero-drop deploys") — the remaining open P0 now that release &
packaging shipped in v0.16.0.

---

## 1. The gap today

On `SIGTERM` the daemon tears down **immediately**. `Runtime::run(shutdown)`
(`bins/siphon-ai/src/runtime.rs:795`) awaits the signal, then:

- aborts the SIP listener tasks (`handle.abort()`, runtime.rs:803-808),
- stops the registration manager + HTTP servers + HEP worker,
- drops the UAS / transaction-manager / socket `Arc`s so per-call tasks
  "see their channels close and exit on their own" (runtime.rs:839-846).

The code says the quiet part out loud (runtime.rs:842-843):

> `// v1 doesn't have a "drain calls cleanly" story; that's a follow-up`
> `// alongside SIGTERM-with-grace.`

Consequences for a real deployment:

- **Active calls are killed mid-conversation.** Per-call tasks don't end
  cleanly — they observe a closed channel and bail. No `BYE` is sent to the
  SIP peer and no `hangup` to the WS server, so the caller hears dead air and
  the upstream switch waits on its session timer to notice.
- **`/ready` never goes false.** `ReadinessFlag`
  (`crates/telemetry/src/readiness.rs`) is marked ready once at startup
  (runtime.rs:754) and is **never flipped back** in normal operation. A load
  balancer / k8s readiness gate keeps routing new calls to a pod that's about
  to die.
- **No rolling deploys.** With `SIGHUP` reload (0.12.0) already in place,
  draining is the missing half: today every deploy/restart drops live calls.

The good news: most of the primitives exist. `CallRegistry`
(`crates/core/src/registry.rs`) is a process-wide map of live calls with
`len()` / `is_empty()` / `snapshot_call_ids()`, and each entry holds a
`CallHandle` — an `Arc<Notify>` that is *already* how BYE/CANCEL ask a call
task to shut down (registry.rs:1-28). `ReadinessFlag` already has
`mark_not_ready()`. The `CALLS_ACTIVE` gauge already counts live calls
(`crates/core/src/acceptor.rs`). This theme is mostly **wiring a drain phase
into `run()`** plus one new reject path — not new per-call machinery.

## 2. Goals / non-goals

**Goals**
1. **Drain on `SIGTERM`.** Enter a *draining* state instead of tearing down:
   flip `/ready` to not-ready, stop accepting new calls, let in-flight calls
   finish on their own, bounded by a configurable timeout, then exit.
2. **Reject new inbound INVITEs while draining** with a retryable SIP response
   (so an upstream proxy routes elsewhere), complementing the `/ready` flip
   (which a load balancer notices only on its next poll).
3. **Clean termination of stragglers** at the deadline: calls still up when
   the timeout expires are ended *gracefully* (`BYE` to the peer, `hangup` to
   the WS) — never the current abrupt channel-drop.
4. **Observable drain.** Drain state + progress on `/admin`, a metric, and
   lifecycle logs.

**Non-goals (this theme)**
- **No call migration / handoff** to another node. Draining lets calls *end*;
  it does not move them. (WS reconnect, 0.7.3, is a different mechanism.)
- **No protocol/CDR/config-schema break.** One additive `[shutdown]` config
  block; protocol stays v1. (A CDR `termination_cause` value for
  drain-forced calls is a small additive option — see §4.)
- **No change to calls that finish within the window** — they behave exactly
  as today.
- **Not a generic "pause/resume traffic" admin control.** Drain is tied to
  the shutdown lifecycle. (An admin-triggered drain-without-exit is a
  plausible *future* extension; out of scope here.)

## 3. Design

### 3.1 A drain phase inside `run()`

Today `run()` is `shutdown.await; teardown`. Insert a drain phase between:

```text
shutdown.await                      // SIGTERM / SIGINT
─► enter draining:
     drain.begin()                  // set the flag (see §3.2)
     readiness.mark_not_ready()     // /ready → 503; LB stops routing
     log + metric: draining started, N active
─► wait for calls to drain:
     loop until registry.is_empty() OR deadline (drain_timeout):
       sleep(poll_interval); log remaining periodically
─► deadline reached with stragglers:
     for each remaining call: graceful terminate (§3.4)
     brief grace for the BYEs to flush
─► existing teardown (runtime.rs:803-846), unchanged
```

The wait is a **poll of `registry.is_empty()`** on a short interval
(~250ms-1s). Draining is a once-per-process, off-the-hot-path event, so
polling is simpler than threading a "last call ended" `Notify`/`watch`
through the registry — but that's decision 3 in §4.

### 3.2 The drain flag

A small shared flag, shaped like the existing `ReadinessFlag` (an
`Arc<AtomicBool>` newtype), constructed in `Runtime` build and cloned into
(a) the `run()` drain logic and (b) the inbound INVITE handler (§3.3).

Open question: reuse `ReadinessFlag` (drain ⇒ not-ready, one flag) or a
separate `DrainFlag`. Leaning **separate** — "not ready" and "actively
draining, reject new work" are distinct states (a node could be not-ready at
startup without draining), and the INVITE handler wants the precise "are we
draining" signal. The `/ready` flip is then just one *action* drain takes,
not the drain flag itself.

### 3.3 Reject new INVITEs while draining

In `crates/sip-glue/src/handler.rs`, `on_invite` already gates inbound calls
(trunk allowlist → 403 via `UserAgentServer::create_response(request, 403,
"Forbidden")`, handler.rs:357-373). Add an earlier check: **if draining and
this is a new (out-of-dialog) INVITE**, respond with a retryable code and
return — before trunk/route work.

- **Only new dialogs are rejected.** In-dialog requests (re-INVITE for
  hold/resume, ACK, BYE) for *existing* draining calls must still flow, or we
  break the very calls we're draining. The handler distinguishes new vs
  in-dialog.
- **Response code = decision 1** (§4). Recommendation: **`503 Service
  Unavailable` + `Retry-After`** (RFC 3261 §21.5.4) — the correct "this node
  is going away, try again / elsewhere" posture for a proxy/LB. `486 Busy
  Here` (the other code the roadmap named) means "user busy" and is a poorer
  fit for node drain.

### 3.4 Graceful termination of stragglers

At the deadline, for each Call-ID still in the registry: end it cleanly
rather than dropping its channel. Two mechanisms already exist —

- the `CallHandle` (`Arc<Notify>`) the registry hands out, which is how
  BYE/CANCEL already trigger a controller to shut down (registry.rs:4-9), and
- `DialogTerminator` (imported in `crates/core/src/registry.rs:43` from
  sip-glue), which sends an actual `BYE`.

**Open implementation question (chunk-2 spike):** does notifying the
`CallHandle` cause the controller to send a `BYE` to the peer + `hangup` the
WS, or only to stop locally? If notify alone doesn't emit a `BYE`, the drain
loop drives `DialogTerminator` for the SIP leg. Either way the outcome must
be: peer gets a `BYE`, WS gets a clean close — not a silent RTP stop. This is
the one part that needs reading the controller teardown path before coding.

### 3.5 Signals: SIGTERM vs SIGINT, and the escape hatch

`shutdown_signal()` (`bins/siphon-ai/src/main.rs:390`) currently resolves on
*either* SIGTERM or SIGINT and treats them identically. For drain:

- **SIGTERM ⇒ drain** (k8s sends SIGTERM, then SIGKILL after
  `terminationGracePeriodSeconds`).
- **A second signal during drain ⇒ force immediate teardown** (operator
  Ctrl-C twice, or k8s' final SIGKILL — though SIGKILL isn't catchable, so
  this mainly serves interactive use). This needs `run()` to keep listening
  for a second signal *while* draining (a `tokio::select!` over "drain
  complete" vs "second signal"), rather than today's already-consumed future.

Decision 2 (§4): does **SIGINT also drain**, or stay fast-exit for dev
ergonomics? Recommendation: both drain, second signal forces — uniform and
safe, with the escape hatch preserving Ctrl-C-now.

### 3.6 Config

New `[shutdown]` block (`crates/config`):

```toml
[shutdown]
# Max time to let active calls finish on SIGTERM before forcing a
# clean teardown. 0 = no drain (immediate exit, today's behavior).
drain_timeout_secs = 30
# SIP response to new INVITEs while draining (decision 1).
# drain_response = "503"   # 503 Service Unavailable + Retry-After
```

`drain_timeout_secs` default = **30s** (decision 4), a middle ground that fits
common k8s grace periods. **It must be ≤ the pod's
`terminationGracePeriodSeconds`** or k8s SIGKILLs mid-drain — documented in
DEPLOY.md (chunk 3). Config home = decision 5 (a `[shutdown]` table vs folding
into `[node]`); recommend a dedicated `[shutdown]` block.

## 4. Decisions — LOCKED (2026-06-24)

1. **New-INVITE reject code = `503 Service Unavailable` + `Retry-After`.**
   The "this node is going away, route elsewhere / retry" posture (RFC 3261
   §21.5.4) for an upstream proxy/LB. `486 Busy Here` rejected (wrong
   semantic). Only *new, out-of-dialog* INVITEs are rejected; in-dialog
   requests for draining calls still flow.
2. **SIGINT drains like SIGTERM; a second signal forces immediate teardown.**
   Uniform behavior for both signals, with Ctrl-C-twice (and a re-sent
   SIGTERM) as the escape hatch. `run()` `tokio::select!`s "drain complete"
   against "second signal" while draining.
3. **Drain-wait = poll `registry.is_empty()`** on a short interval
   (~250ms–1s) until empty or deadline. Simple, off the hot path; no new
   signalling threaded through the registry. (A completion `Notify`/`watch`
   was rejected as overkill for a once-per-process event.)
4. **`drain_timeout_secs` default = `30`.** `0` always means "no drain /
   immediate exit" (today's behavior, opt-out).
5. **Config home = a dedicated `[shutdown]` table.**

(Also locked as defaults: a separate `DrainFlag` distinct from
`ReadinessFlag`; reject only out-of-dialog INVITEs; graceful `BYE` + WS
`hangup` for stragglers at the deadline.)

## 5. Observability / tests

**Observability** (ships with the feature, per CLAUDE.md §4.5):
- **Metrics:** `siphon_ai_draining` gauge (0/1); `siphon_ai_drain_seconds`
  histogram (observed once when drain finishes);
  `siphon_ai_calls_drain_forced_total` counter (calls ended by the deadline
  rather than naturally). `CALLS_ACTIVE` already exists for "how many left".
- **`/admin`:** a drain block in the status/health response — `draining:
  bool`, `active_calls`, `deadline`/`remaining_secs`.
- **Logs:** `info` "drain started (N active, timeout Xs)", periodic `info`
  "draining: N remaining", and `info` "drain complete in Xs" /
  `warn` "drain timeout — force-terminated N calls".
- **CDR (optional, additive):** a `termination_cause` value for
  drain-forced calls so the outcome is attributable. Bumps `CDR_VERSION` if
  added — gate on whether it's wanted (small, high-signal).

**Tests:**
- Unit: drain flag flips `/ready` and makes `on_invite` return the chosen
  code for a *new* INVITE while still allowing an in-dialog request.
- Runtime: a `run()` test with N fake registry entries that clear before the
  deadline (exits when empty, no force) and one that doesn't (force path
  fires, bounded by timeout).
- Integration (SIPp, `test-harness/`): call up → send SIGTERM → assert the
  daemon sends `BYE` and exits within the timeout; a *new* INVITE during
  drain gets the reject code; a short-call that ends on its own before the
  deadline drains with zero forced terminations.

## 6. Chunks (target ~v0.17.0)

1. **Drain core.** ✅ **DELIVERED.** `[shutdown].drain_timeout_secs` + the
   drain phase in `run()` (separate `DrainFlag`, `mark_not_ready`,
   poll-until-empty-or-deadline on a 250 ms tick), reject new out-of-dialog
   INVITEs with `503 Service Unavailable` + `Retry-After`, `siphon_ai_draining`
   gauge + `siphon_ai_drain_seconds` histogram, lifecycle logs. Calls that
   finish within the window drain cleanly; stragglers are still dropped at the
   deadline (parity with today for the timeout case, but now bounded and
   observed). `[shutdown]` is restart-required on reload.
2. **Graceful straggler termination.** Read the controller teardown path
   (§3.4 spike), then end deadline-survivors with a real `BYE` + WS `hangup`
   (via `CallHandle` notify and/or `DialogTerminator`),
   `calls_drain_forced_total`, second-signal-forces escape hatch, optional
   CDR `termination_cause`.
3. **Surface + docs + release.** `/admin` drain status, `docs/DEPLOY.md`
   (rolling-deploy guidance: drain ≤ `terminationGracePeriodSeconds`, the
   `/ready` + 503 interplay, `preStop`/systemd notes), `docs/OPERATIONS.md`
   runbook line, CHANGELOG, tag ~v0.17.0.
