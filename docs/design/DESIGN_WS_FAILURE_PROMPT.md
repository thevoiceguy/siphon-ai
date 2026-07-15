# Design note — WS-failure prompt playback

> **Status: APPROVED — decisions LOCKED (§9, all per recommendation,
> 2026-07-15).** Pins the failure
> taxonomy, the config surface, and the tap/controller mechanics before
> implementation — the usual design-first pass. The build follows this;
> deviations get noted back here. Target: **0.34.0**. No WS-protocol,
> CDR, or webhook-schema changes.

Today a WS failure ends the call with a **bare hangup**: the bot goes
silent and the caller gets disconnected with no explanation — the worst
possible experience for the one moment the platform (not the caller,
not the operator) is at fault. This theme finishes a switch that has
existed since v1: `on_ws_failure = "hangup"` shipped with
`"play_prompt"` *reserved*, blocked on "a forge-driven prompt player
that isn't built" (`compile.rs`). That player has since been built —
the 0.26.0 consent-announcement machinery (`AnnounceSource` +
`TapCommand::Announce`, driven off the tap's 20 ms tick) is exactly a
one-shot prompt player. This theme wires it to the failure paths:

> WS dies → caller hears *"we're experiencing technical difficulties,
> please call back"* → normal SIP teardown (BYE).

---

## 1. Which failures play the prompt

The prompt covers every teardown where **SiphonAI ends the call because
the WS became unusable** — the cases where the caller would otherwise
experience an unexplained drop:

| Path | Today | With `play_prompt` |
|---|---|---|
| Unexpected WS drop, reconnect off / drop not eligible | immediate teardown | prompt → teardown |
| WS connect failure at call answer (server unreachable — the `DEV_PLAN` §14 acceptance case) | immediate teardown | prompt → teardown |
| Keepalive timeout, `protocol_error`, `server_too_slow` | `error` + `stop` (best-effort) → teardown | same, then prompt → teardown |
| WS-reconnect window exhausted (0.7.3 give-up) | teardown off MOH | prompt → teardown (§3) |

**Not** covered — the WS side ended the call on purpose, or the
teardown isn't WS-related: server `hangup` / clean `stop`, caller
hangup/CANCEL, transfers, park (`stop{park}` then detach), tap-side
teardowns (`rtp_timeout` — the caller's media is already gone),
admin hangup, and drain force-termination (deploys should stay fast;
§9.1). Mechanically: the prompt hooks the controller paths that
currently set `CallTermination::BridgeEnded` from a bridge-task
outcome that is not `StopSent`/`ControllerHungUp`.

CDR termination causes are **unchanged** — the prompt is a courtesy on
the way out, not a new outcome (`duration_ms` grows by the prompt
length; noted in docs).

---

## 2. Config surface

```toml
[bridge]
on_ws_failure = "play_prompt"          # existing key, new value; default stays "hangup"
ws_failure_prompt_file = "/etc/siphon-ai/failure-8k.wav"   # NEW

[[route]]
[route.bridge]
on_ws_failure = "hangup"               # per-route override (key already exists)
ws_failure_prompt_file = "..."         # NEW per-route override
```

- `on_ws_failure` today exists **per-route only**, with `"hangup"` the
  sole accepted value. This theme adds the **global `[bridge]` default**
  (routes inherit field-wise, like everything else) and accepts
  `"play_prompt"` in both places.
- `ws_failure_prompt_file` — a WAV at the bridge rate, same rules as
  the consent announcement (8/16 kHz mono, no resampler upstream).
  **Load-time validation**: when any effective `on_ws_failure` is
  `play_prompt`, the effective file must be set and exist — fail loud
  at startup (CLAUDE.md §4.6). Sample-rate compatibility is per-call
  (the rate isn't known until codec negotiation) — see §4 fail-open.
- Outbound (originated) calls resolve from the global `[bridge]`
  defaults, same as every bridge knob.

---

## 3. Mechanics

### 3.1 Tap survival after the drop

The prompt needs the tap alive after the WS-facing channels close —
exactly what the 0.7.3 `survive_ws_drop` tap mode provides for
reconnect. The acceptor enables that mode when the route's effective
`on_ws_failure = "play_prompt"` **or** reconnect is on (today's
condition). The teardown-authority semantics that come with it
(`commands_rx` closing = teardown) are already handled by the
controller for reconnect calls; prompt calls ride the same path.
(`MediaTap::with_ws_reconnect` gets a truthful rename to
`with_survive_ws_drop`; internal API, no compat concern.)

### 3.2 Controller flow

On a §1-eligible failure, instead of `termination = BridgeEnded; break`:

1. **Lazy-load** the `AnnounceSource` (file + call sample rate) at
   failure time. Teardown is control-plane — the file I/O is fine
   here, and lazy loading avoids holding a decoded prompt in RAM for
   every healthy call.
2. `TapCommand::Announce { source, done }` — the same one-shot player
   the consent prompt uses; the tap plays it on the 20 ms tick and
   fires `done(ms_played)` at EOF.
3. Wait on `done` racing: **caller hangup / tap end / shutdown / a
   30 s safety cap** (a wedged tap must not wedge teardown; the cap is
   fixed, not a knob — a failure prompt longer than 30 s is a config
   smell, warned at load).
4. Then the existing teardown (BYE, CDR, webhooks) with the
   termination cause it would have had anyway.

### 3.3 The reconnect-exhausted path (announce over park)

During a reconnect window the tap is **parked on MOH**, and
`TapCommand::Announce` today *skips* when parked ("park owns the
caller's ear"). One small, ordered tap change makes the give-up prompt
work:

- An `Announce` that arrives **while parked** now plays: the 20 ms
  tick prefers announcement frames over MOH until EOF, then returns to
  MOH (which teardown then stops). MOH → prompt → BYE is exactly the
  right caller experience after a failed reconnect.
- The **reverse order is unchanged**: a `Park`/`Hold` arriving while an
  announcement plays still cuts the announcement short (the 0.26.0
  consent semantics stay intact — pinned by existing tests).

### 3.4 Failure-of-the-failure-path

Everything degrades to today's behavior (immediate teardown), never to
a wedged call: unusable prompt file at call time (rate mismatch, file
replaced by garbage after load-check) → warn + plain hangup; tap
already gone → plain hangup; caller hangs up mid-prompt → cut and tear
down. The prompt is a courtesy — **fail-open**, deliberately unlike
the consent announcement's fail-closed rule (skipping a compliance
prompt breaks the law; skipping an apology just loses politeness).

---

## 4. Observability (same PR as the feature)

- **Metric** — `siphon_ai_ws_failure_prompts_total{result}` with
  `result ∈ played | cut_short | unusable | timeout` (bounded; no
  route/name label — route-level detail is in logs).
- **Logs** — `info!` prompt start (with the failure class) and finish
  (ms played); `warn!` on unusable/timeout.
- **CDR / webhooks / WS protocol** — no schema changes. The WS is dead
  by definition; the existing `error`/`stop` best-effort emissions
  (0.14.0) are unchanged and happen *before* the prompt where they
  apply.

---

## 5. Testing

- **Tap unit** (`media-glue/tests/tap.rs`) — announce-while-parked
  plays frames and returns to MOH at EOF; park-cuts-announce is
  unchanged (extend the existing 0.26.0 fixtures).
- **Controller integration** (`core/tests/`, the WS-harness pattern
  from `controller_lifecycle.rs`) — server drops the socket mid-call
  with `play_prompt` configured: the tap receives `Announce`, teardown
  waits for `done`, cause is unchanged; a second test with the prompt
  file unusable asserts the plain-hangup fallback.
- **Config** — load-time matrix: `play_prompt` without a file fails,
  route inherit/override resolution, unknown value still rejected.
- **SIPp phase** — reuse the `ws_reconnect` phase's `--drop-after-ms`
  echo-server flag (0.7.3): caller up → WS dropped → assert the call
  stays up for ≈ the prompt duration (a runtime-generated tone WAV,
  same no-binary-in-repo rule as `gen_tone_pcap.py`) and then gets a
  BYE; assert `ws_failure_prompts_total{result="played"}` ticked.

---

## 6. Docs

- `docs/CONFIG.md` — `on_ws_failure` gains `"play_prompt"` +
  `ws_failure_prompt_file` (global + route rows); drop the
  "v1 only supports hangup" caveat.
- `docs/DEPLOY.md` — metric row.
- `docs/DEV_PLAN.md` — footnote the §14 acceptance case as now
  supporting both policies (doc-drift rule, CLAUDE.md §9).
- `docs/ROADMAP.md` — mark the P2 item delivered (release PR).

---

## 7. Explicitly out of scope

- **Prompts for non-WS terminations** (`rtp_timeout`, drain, admin
  hangup) — the same hook could serve them later, but the ROADMAP item
  is WS failure; a general "teardown prompt policy" is a different,
  bigger design.
- **Per-failure-class prompt files** (different message for
  server-too-slow vs drop) — one file in v1.
- **TTS / multi-rate auto-resampling** — provide the file at the
  bridge rate, same as MOH and the consent prompt.

---

## 8. Delivery

One feature PR (config + acceptor + tap + controller + tests + SIPp
phase + docs), then the release PR (0.34.0 bump + CHANGELOG + ROADMAP).
Same cadence as 0.33.0.

---

## 9. Decisions to lock

1. **Failure taxonomy (§1)** — prompt on all siphon-initiated
   WS-unusable teardowns (drop, connect-at-answer, keepalive,
   protocol_error, server_too_slow, reconnect-exhausted); never on
   server-intended endings, caller actions, tap-side causes, or drain.
   **Recommend as stated** — drain especially: deploys must not grow a
   30 s tail per call.
2. **Reconnect give-up plays the prompt**, via the ordered
   announce-over-park tap change (§3.3). **Recommend yes** — it's the
   highest-value case (the caller already invested the MOH wait), and
   the ordering rule keeps consent semantics untouched.
3. **Fail-open** on an unusable prompt at call time (plain hangup +
   warn + `result="unusable"`), unlike consent's fail-closed.
   **Recommend yes.**
4. **Load-time validation**: `play_prompt` anywhere effective requires
   an existing file; 30 s+ files warn at load. **Recommend yes.**
5. **Fixed 30 s safety cap** on prompt playback at teardown, no config
   knob. **Recommend yes.**
6. **No CDR change** — cause and schema untouched; evidence lives in
   logs + the metric. *(Alternative: an additive optional
   `ws_failure_prompt_ms` field.)* **Recommend no CDR change** —
   revisit if operators ask for per-call evidence.
7. **Global default + per-route override** for both keys (the route
   key already exists; the global is new). **Recommend yes.**
8. **Version 0.34.0, two PRs** (feature, release). **Recommend as
   stated.**
