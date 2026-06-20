# Design: config CLI — check, render, route-test (+ reload?)

Status: **DRAFT — decisions pending** (this note + an AskUserQuestion lock
the forks, then chunked PRs, same cadence as admin-auth → v0.10.0 and
webhook-durability → v0.11.0).

Theme: P1 from the post-delivery-plan list — *"Config operations are
restart-heavy and missing a real validation CLI. Add `siphon-ai check
--config`, rendered config output, route-test tooling, and eventually safe
reload for routes/gateways/webhooks."*

---

## 1. The gap today

The daemon has one job and one shape: `siphon-ai --config X` loads + compiles
the TOML and **runs**. There is no way to:

- **Validate a config without starting the daemon.** Operators (and CI, and
  Ansible/Helm preflight) can't check a file before a deploy. Worse,
  `contrib/README.md:72` documents `siphon-ai --config … --check` — a flag
  that **doesn't exist**, so the documented preflight silently does nothing
  (it's parsed as an unknown arg and clap errors, or is ignored).
- **See the *effective* config.** Config compiles globals + per-route
  overrides + env-expanded `${VAR}`s into a `Config`; an operator can't see
  what their file actually resolved to (which `${VAR}` won the merge, what a
  route inherits vs overrides).
- **Test routing.** The dialplan is order-sensitive first-match-wins
  (`RouteSet::find_match`); there's no way to ask "given a call from X to Y
  on trunk Z, which route wins and what bridge config does it get?" short of
  placing a real call.
- **Reload.** Any change needs a full process restart. `SIGHUP` today only
  hot-swaps the SIP/TLS cert (`spawn_sighup_handler`); routes/gateways/
  webhooks changes require a restart, dropping every in-flight call.

The good news: validation is **already a pure function** —
`siphon_ai_config::load_from_path(path) -> Result<Config, LoadError>` does
read → `${VAR}` expand → serde → `compile()` (all validation, CLAUDE.md
§4.6). `check` is mostly "call that and report"; `print-config` and
`route-test` are renderers over the resulting `Config`.

## 2. Goals / non-goals

**Goals**
1. **`check`** — validate + compile a config file and exit non-zero on any
   error, with a concise OK summary on success. The CI/preflight primitive.
   Fixes the phantom `--check` in `contrib/README.md`.
2. **`print-config`** — render the *effective* compiled config (post-merge,
   post-`${VAR}`), **secrets redacted**, so an operator can see what their
   file resolved to.
3. **`route-test`** — given synthetic call attributes (from/to/RURI/
   register_source/headers), report which route wins (first-match-wins) and
   the effective merged bridge/media/security/recording config for it.

**Non-goals (this theme — see §6 reload decision)**
- A config *server* / dynamic API. These are CLI subcommands of the existing
  binary.
- Changing the config schema. This theme only *reads* config.
- Hot reload of socket-binding sections (`[sip].listen`, `[observability]`,
  `[admin].listen`) — those inherently need a restart.

## 3. Design

### 3.1 CLI shape (decision 1)

The binary gains optional subcommands; **no subcommand = run the daemon**, so
`siphon-ai --config X` (systemd unit, every existing invocation) is
unchanged. `--config` stays a shared top-level arg.

```
siphon-ai --config X                       # run the daemon (today's behavior)
siphon-ai check        --config X          # validate + compile, exit 0/1
siphon-ai print-config --config X [--show-secrets] [--format text|json]
siphon-ai route-test   --config X --to 1000 [--from …] [--ruri-user …]
                                           [--register-source trunk] [-H 'X-K: v']…
```

`check` / `print-config` / `route-test` are read-only, never bind a socket,
and exit when done. `--log` stays run-only (ignored by the tooling
subcommands). Alternative considered — flat flags (`--check`,
`--print-config`, `--route-test-*`) — rejected: route-test alone needs ~6
inputs and the flag soup doesn't extend (see decision 1 in §6).

### 3.2 `check`

Calls `load_from_path`. On `Err(LoadError)` → print the error (the existing
`thiserror` messages are already operator-grade — bad listen, unknown role,
missing register_source, invalid regex, …) to stderr and **exit 1**. On
`Ok(Config)` → print a one-screen summary (node id, sip listen + transports,
route count + whether a default route exists, which optional subsystems are
enabled: outbound/conference/park/recording/cdr/webhooks/hep/admin/
stir_shaken) and **exit 0**. A missing default route prints a warning
(matches the daemon's startup `has_default()` warning) but still exits 0.

### 3.3 `print-config` (rendered effective config)

A bespoke text renderer that walks the compiled `Config` and prints the
effective values — *not* a TOML round-trip (the compiled graph holds
`SocketAddr`, compiled `Regex`, hashed admin tokens, etc. that don't
round-trip). **Secrets are redacted by default**: any `auth_header`,
`secret`, `password`, `capture_password`, gateway credential, or admin token
renders as `<redacted>` when set / `<unset>` when absent. `--show-secrets`
opts out (for local debugging). `--format json` is a follow-up nicety; text
is the default.

### 3.4 `route-test`

Builds a `CallInfo` from CLI flags (defaults: `register_source=trunk`, empty
header set; `--to`/`--from`/`--ruri-user`/`--ruri-host`/`-H key:val` set the
rest), runs `RouteSet::find_match`, and prints the matched route's `name` (or
"no match → SIP 404") plus its effective merged bridge/media/security/
recording config (globals + the route's overrides). This makes the
order-sensitive first-match-wins dialplan testable offline and in CI.

## 4. Redaction set (locked default)

Redacted in `print-config` unless `--show-secrets`:
`[webhooks].auth_header` / `.secret`, `[cdr.webhook].auth_header` /
`.secret`, `[[register]].password`, `[hep].capture_password`,
`[[gateway]]` credentials, `[[admin.token]].token` (already stored hashed —
shown as `<hashed>`). File *paths* (TLS key, trust anchors, MOH file,
recording dir) are not secrets and render verbatim.

## 5. Observability / tests

These are CLI tools, not daemon paths, so no metrics. Tests: a
`bins/siphon-ai/tests/cli.rs` integration test drives the built binary over
fixture TOMLs (valid → exit 0 + summary; invalid → exit 1 + error on stderr;
`route-test` matches the expected route for representative inputs;
`print-config` redacts a secret). Reuse `bins/siphon-ai/tests/fixtures`.

## 6. Decisions — LOCKED (2026-06-19)

1. **CLI shape = optional subcommands**, daemon as the no-subcommand default.
   `siphon-ai --config X` still runs the daemon (systemd + every existing
   invocation unchanged); `check` / `print-config` / `route-test` are
   explicit subcommands. `contrib/README.md`'s phantom `--check` flag becomes
   the `check` subcommand.
2. **Reload is IN this theme** (its own chunk), not deferred. `SIGHUP`
   re-reads the config and hot-applies the reload-safe sections; see §6a.

(Redaction-by-default, daemon-default-run back-compat, text output, and the
read-only/no-schema-change scope for the *tooling* subcommands are defaults.)

## 6a. Reload design (chunk 3)

Extends the existing `spawn_sighup_handler` (which today only hot-swaps the
SIP/TLS cert). On `SIGHUP` (= `systemctl reload siphon-ai`) the daemon
re-reads + recompiles the **same `--config` path**:

- **Fail-safe.** A reload that fails to load/compile is *not applied* — the
  running config stays live, the error is logged, and a metric ticks. A bad
  edit can never take down a running daemon. (This is why `check` matters as
  the pre-reload preflight.)
- **Reload-safe sections, hot-applied atomically** behind a swappable handle
  (e.g. `arc_swap::ArcSwap<...>` or `tokio::sync::watch`): the **route table**
  (consulted per-INVITE in the acceptor), **gateways** (read at originate
  time), and **webhook / CDR sinks**. New calls pick up the new values;
  in-flight calls keep the config they already captured (no retroactive
  remap). The log-filter is already runtime-mutable via the admin API and is
  out of scope here.
- **Restart-required sections.** `[sip]` listen/transports, `[node]`,
  `[observability]`/`[admin]` listeners, and `[media]` bind-time choices
  can't change without rebinding sockets / re-creating the engine. If a
  reload's value for any of these differs from the live one, the reload
  **applies the safe sections and logs a prominent warning** naming the
  section(s) that need a restart — it does not silently pretend they changed.
- **Observability.** `siphon_ai_config_reloads_total{result=applied|failed|
  no_change}` counter + an `info` log summarizing what changed. Each reload
  is auditable.

The mechanic cost is real: the runtime must hold the reloadable bits behind
shared swappable handles instead of owning them outright (the acceptor's
`RouteSet`, the outbound service's gateways, the webhook/CDR sink handles).
That refactor is the bulk of the chunk.

## 7. Chunks (target ~v0.12.0)

1. **`check` + CLI restructure.** Optional-subcommand CLI (daemon = default),
   `check` (validate + summary + exit code), fix `contrib/README.md` (+ any
   other doc) to the real `check` subcommand, CLI integration tests. The
   cheap quick-win.
2. **`print-config` + `route-test`.** Effective-config renderer (redaction +
   `--show-secrets`) and the offline route matcher; tests.
3. **Reload.** `SIGHUP` re-read + fail-safe apply of routes/gateways/webhooks
   behind swappable handles; restart-required warning for socket sections;
   `config_reloads_total` metric; tests (good reload applies; bad reload
   keeps the old config; socket-section change warns).
4. **Docs + release.** `docs/CONFIG.md` ("Validating, inspecting & reloading
   config"), `docs/DEPLOY.md` / `OPERATIONS.md` preflight + `systemctl
   reload` guidance, CHANGELOG, tag ~v0.12.0.
