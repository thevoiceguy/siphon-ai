# Design note — Sightglass: a terminal UI for siphon-ai

> **Status: IN PROGRESS.** PR 1 (this doc, the `admin-api-types`
> extraction, the multi-node read-only scaffold) and PR 2 (operator
> actions with node-named confirm modals, toasts, per-node RBAC-aware
> keybinds via startup role probes, `--read-only` enforcement, user
> guide `docs/SIGHTGLASS.md`) are implemented. PRs 3–6 in §10 are
> pending. Update this header as chunks land.

A sight glass is the fitting on a pipe that lets you watch the fluid
moving through it. `sightglass` is that for running siphon-ai nodes —
one or many: a ratatui terminal application that fans out to each
configured node's admin API and renders a live, tabbed operator
console — trunk status, active calls with per-call media detail,
system health, errors — plus the operator actions the admin API
already supports (hangup, park/retrieve, originate, registration
refresh, live log-level control). A single node is just the fleet of
size one; multi-node is first-class from PR 1, not a bolt-on.

---

## 1. Naming and placement

- **Crate:** `siphon-ai-sightglass`, at `bins/sightglass/` in this
  workspace, next to `bins/siphon-ai/`.
- **Binary:** `sightglass` (via `[[bin]] name`), k9s-style: the binary
  claims nothing in the `siphon-*` namespace.
- The `siphon-ai-*` crate prefix follows the existing convention
  (`siphon-ai-bridge`, `siphon-ai-http`, `siphon-ai-testkit`). The bare
  `siphon-*` prefix stays reserved for platform-level tools (siphon-rs,
  forge-media, hep-rs lineage) and the future scriptable CLI keeps
  `siphon-ai-ctl` available — sightglass is not that tool. Sightglass
  itself covers the multi-node view (§2), so no separate fleet-monitor
  name needs reserving.

## 2. Architecture: a fleet-aware, pure admin-API client

```
                                          ┌─────────────────────┐
┌──────────────┐   HTTPS + bearer token   │ siphon-ai [admin]   │
│  sightglass  │ ───────────────────────► │  prod-1             │
│  (ratatui)   │ ───────────────────────► │  prod-2   …  ×N     │
└──────────────┘   one poller set/node    └─────────────────────┘
```

The TUI runs **out of process** — possibly on a different machine —
and speaks only to each node's `[admin]` HTTP listener. It is a
rendering client with zero privileged access:

- No daemon embedding. The daemon is headless under systemd; a TTY in
  the daemon is a liability and touches nothing it should.
- No new aggregation layer. The admin API is already the sanctioned
  cross-call view (CLAUDE.md §4.4 stays intact — sightglass sees what
  any admin client sees, nothing more).
- The daemon's dependency tree is untouched: ratatui/crossterm live
  only in `bins/sightglass/Cargo.toml`.

**Fleet model.** Multiple nodes are a client-side concern only —
daemons stay entirely unaware of each other (CLAUDE.md §4.4 is
untouched; there is no cross-node state anywhere server-side):

- Every record in the TUI's state carries a `NodeId`. Call ids,
  registration names, and room ids are unique **per node only** — the
  composite key is always `(node, id)`, and every action dispatches to
  the owning node's client. This is baked into the data model in PR 1;
  it is the one thing that cannot be retrofitted cheaply.
- One independent poller set per node, start times staggered so N
  nodes don't burst-poll in phase. A slow or down node degrades only
  its own rows — it never stalls rendering or other nodes' pollers.
- Node reachability is itself first-class state: Overview shows a
  fleet health grid, and a down node renders as `○ down (retrying)`
  with its last-seen data greyed, never as an error screen.
- Auth is **per node** (each node has its own token, possibly a
  different role). Action keys grey out per row based on the owning
  node's role, so a mixed fleet (operator on staging, readonly on
  prod) behaves correctly.

**Polling, not push (v1).** Per node: overview + calls list +
registrations poll at 1 s; the per-call stats endpoint is polled
**only for the focused call**, never fanned out across all calls. The
TUI accumulates history
client-side (ring of ~120 samples) for sparklines; the daemon stores
nothing new. SSE/WS push on the admin listener is a possible v2 if
polling ever proves annoying — it is explicitly out of scope for v1.

**Shared response types.** The admin response shapes currently live as
ad-hoc `json!` in `crates/telemetry/src/admin.rs`. PR 1 extracts them
into a small `crates/admin-api-types` crate (serde structs only, no
heavy deps) used by both the daemon and sightglass, so the client can
never drift from what the server serializes. Existing wire shapes are
preserved byte-for-byte — this is a refactor, not an API change
(snapshot tests in `telemetry` guard it).

## 3. Non-goals

- Not a softphone; no SIP or RTP in the TUI.
- Not the scriptable CLI (`siphon-ai-ctl`, future work, separate tool).
- No central aggregator service. Multi-node view is client-side
  fan-out from one sightglass process; nodes never learn about each
  other, and no daemon grows fleet awareness.
- No cross-node operations. Every action targets exactly one node;
  there is no "drain all", no fleet-wide bulk hangup.
- No config editing. Read-and-act only.
- No historical analytics — Homer/Grafana own history; sightglass owns
  "right now" plus a short in-memory tail.

## 4. Tabs and their data sources

All tables are fleet-unified: rows from every configured node merge
into one view with a **Node** column (hidden automatically when only
one node is configured), and `n` cycles a node filter
(all → prod-1 → prod-2 → …) that scopes every tab at once.

| Tab | Contents | Source (per node) | Exists today? |
|---|---|---|---|
| **Overview** | fleet health grid (one row per node: reachability, version, uptime, active calls, drain state, HEP collector up) + aggregate totals and calls/sec sparkline | `GET /admin/v1/status` (new, §6.2), `GET /admin/v1/drain` | partly |
| **Trunks** | registrations across the fleet, grouped by node: state, expiry, last result; refresh/restart actions | `GET /admin/v1/registrations`, `POST …/{name}/refresh\|restart` | yes |
| **Calls** | unified table of active calls (node, both id namespaces + direction per #311); detail pane for focused call: codec, duration, MOS gauge, jitter/loss sparklines, WS state; actions §5 | `GET /admin/v1/calls`, `GET …/{id}/stats` | yes |
| **Rooms** | conferences + participants, parked calls, node-tagged; end room, kick, retrieve | `GET /admin/v1/conferences`, `GET /admin/v1/parked`, conference sub-resources | yes |
| **Errors** | merged fleet tail of warn/error events, node-tagged, with call_id correlation | `GET /admin/v1/errors` (new, §6.1) | no |
| **System** | acts on one node at a time (selected via the node filter): log filter get/set, HEP test probe, drain status/initiate | `PUT /admin/v1/log`, `POST /admin/v1/hep/test`, `GET /admin/v1/drain` | mostly |

## 5. Operator actions (all against existing endpoints)

| Key | Action | Endpoint | Min role |
|---|---|---|---|
| `x` | hang up focused call (confirm modal) | `POST /admin/v1/calls/{id}/hangup` | operator |
| `p` | park focused call | `POST /admin/v1/calls/{id}/park` | operator |
| `u` | retrieve parked call — optional new `ws_url` (move a live call to a fallback WS server) | `POST /admin/v1/calls/{id}/retrieve` | operator |
| `o` | originate (dial form modal) | `POST /admin/v1/calls` | admin |
| `c` | add focused call to a conference | `POST /admin/v1/conferences/{id}/participants` | operator |
| `r` / `R` | refresh / restart focused registration | `POST /admin/v1/registrations/{name}/…` | operator |
| `L` | set log filter (System tab) | `PUT /admin/v1/log` | operator |
| `n` | cycle node filter (all → node → …) | — (client-side) | — |

Rules:

- **Actions are node-scoped.** Every action resolves through the
  focused row's `(node, id)` key and dispatches to that node's client;
  confirm modals name the node ("hang up abc@host **on prod-2**?") so
  a fleet operator never acts on the wrong box.
- **RBAC-aware keybinds, per node.** On connect, sightglass learns
  each node's token role (`crates/telemetry/src/auth.rs`:
  `readonly` < `operator` < `admin`) and greys out actions the owning
  node's token cannot perform, rather than surfacing a 403 after the
  keypress. *Mechanism (settled in PR 2):* the 403 body carries no
  role, so the role is learned by probing the RBAC gate — which runs
  before dispatch — with two side-effect-free POSTs: a hangup on a
  sentinel call id (403 ⇒ readonly, 404 ⇒ ≥ operator) and an
  empty-body originate (403 ⇒ operator, validation 400/501 ⇒ admin;
  nothing is dialed). Probes are skipped under `--read-only`, and an
  unlearned role stays permissive — a later real 403 toasts and
  teaches the ceiling.
- **`--read-only` flag** disables all mutating actions client-side
  regardless of token role — for NOC wall screens.
- Destructive actions (hangup, end-room, drain) always get a
  confirmation modal. Everything flows through the existing admin
  audit stream, attributed to the token — a deliberate reason to do
  call-kill via the admin API rather than any side channel.

## 6. Daemon-side work (each its own small PR, observability rules apply)

### 6.1 Recent-errors ring buffer + `GET /admin/v1/errors`

A `tracing` `Layer` in `crates/telemetry` captures `warn`/`error`
events into a bounded ring (default 256, config-tunable). Off the hot
path by construction — it is a subscriber layer; audio tasks never
block on it (same non-blocking discipline as HEP, §4.7). Entries carry
timestamp, level, target, message, and `call_id` when present in the
span, so the Errors tab can jump-link to the call. Endpoint returns
the ring newest-first; `readonly` role suffices.

### 6.2 `GET /admin/v1/status` summary

Small JSON: version, uptime_secs, active_calls, total_calls,
registrations `{up, down, total}`, `hep_collector_up`, drain state.
Everything already exists as metrics/state; this avoids the TUI
parsing Prometheus text exposition. `readonly`.

### 6.3 Recent-calls ring + `GET /admin/v1/cdrs/recent`

In-memory ring (default 50) of completed-call CDR summaries
(disposition, duration, hangup cause, MOS). Feeds a history section on
the Calls tab. Reuses the CDR structs from `crates/cdr` — no second
schema. `readonly`.

### 6.4 Stats enrichment (additive fields on `…/{id}/stats`)

Fields `CallController`/tap already know, plumbed into the stats
response: VAD state (speaking/silent), last DTMF digits, WS reconnect
count, recording state, STIR/SHAKEN verstat, SRTP on/off. Additive
JSON — no version concerns.

### 6.5 `POST /admin/v1/drain` (admin role)

Programmatic equivalent of SIGTERM drain so the System tab can start a
graceful drain with a confirm modal. Reuses the existing shutdown
path; no new drain logic.

### 6.6 Open question — gateway (IP-auth) trunk health

Registrations have live state; static outbound gateways do not. V1
shows configured gateways with last-used info only. Active OPTIONS
probing toward gateways would be a real feature (likely touching
siphon-rs) — deliberately deferred, tracked separately.

## 7. Visual and interaction design

**Discipline over decoration.** One theme, used everywhere:

- A single palette struct (Catppuccin-adjacent: one accent, one dim,
  semantic green/amber/red). No ad-hoc `Color::` in widget code.
- Status is always dot + color (`●` up, `◐` degraded, `○` down), never
  color alone (colorblind-safe).
- Chrome: header (node name, version, uptime, active calls), tab bar
  with accent underline, **context-sensitive footer** showing only the
  keybinds valid right now.
- Motion: braille sparklines/charts for calls-per-second and MOS trend;
  `Gauge` for MOS; request-in-flight throbber; action-result toasts
  ("hangup accepted (202)") that fade; centered modals over a dimmed
  background.
- Input: vim keys and arrows; `/` fuzzy-filter on tables; sortable
  columns; crossterm mouse support (click tabs/rows).
- Layout collapses detail pane below the table on narrow terminals;
  nothing truncates silently.
- Unicode/nerd-font glyphs degrade to ASCII via a `--ascii` flag.

**Responsiveness:** tokio throughout. Poller tasks feed the render
loop over channels; redraw on data or input; **no blocking call ever
in the draw path**. Unreachable admin API renders a clear full-screen
"can't reach <target> — is `[admin]` enabled?" state, retrying with
backoff, never a stack trace.

## 8. Configuration and connection

`~/.config/sightglass/config.toml` (TOML, consistent with house
rules) defines the fleet — one `[[node]]` per daemon, order = display
order:

```toml
poll_interval_ms = 1000   # per node
theme = "default"

[[node]]
name = "prod-1"
url = "https://prod-1.example.com:9090"
token_file = "~/.config/sightglass/prod-1.token"
# ca = "/etc/ssl/private-ca.pem"   # optional, private CAs

[[node]]
name = "prod-2"
url = "https://prod-2.example.com:9090"
token_file = "~/.config/sightglass/prod-2.token"
```

- `node.name` must be unique — it is the `NodeId` shown in every Node
  column and confirm modal.
- Ad-hoc single-node use needs no config file:
  `sightglass --target https://host:9090 --token-file ./token`
  (defines an anonymous one-node fleet; mutually exclusive with the
  config's node list).
- TLS: respects each admin listener's TLS; per-node `ca` for private
  CAs. Tokens via file or `SIGHTGLASS_TOKEN` env (single-node only) —
  never as a CLI arg (visible in `ps`).
- Requires the `[admin]` listener enabled on each daemon (omitted ⇒
  not served); setup docs lead with this.

## 9. Dependencies (new, confined to `bins/sightglass`)

`ratatui`, `crossterm`, `tui-textarea` (originate form), plus the
existing workspace `reqwest`/`siphon-ai-http` machinery for HTTP. None
of these appear in any daemon crate. The release pipeline builds it
with the existing musl/zigbuild flow — it is a pure HTTP client, so it
needs none of the libopus static-link handling; ships in the existing
.deb alongside the daemon and as its own GHCR artifact.

## 10. Build plan (PR sequence)

| PR | Contents | Daemon touched? |
|---|---|---|
| **1** | this doc; `crates/admin-api-types` extraction (wire-preserving); `bins/sightglass` scaffold: theme, chrome, event loop, **multi-node config + `NodeId` data model + per-node pollers + node filter/health states**, Overview/Trunks/Calls tabs read-only against existing endpoints | refactor only |
| **2** | Calls-tab actions (hangup/park/retrieve/originate/conference) + confirm modals (node-named), toasts, per-node RBAC-aware keybinds, `--read-only` | no |
| **3** | §6.1 errors ring + endpoint; Errors tab with live tail | yes (small) |
| **4** | §6.2 status endpoint; Overview tab completed; Rooms tab | yes (small) |
| **5** | §6.3 recent-CDR ring; history section; §6.4 stats enrichment; detail pane completed | yes (small) |
| **6** | §6.5 drain endpoint; System tab (log filter, HEP probe, drain) | yes (small) |

Each daemon-side PR carries its own metrics/logs/docs per CLAUDE.md
§4.5; endpoint additions are documented in `docs/DEPLOY.md` in the
same PR. Sightglass gets a user-facing guide (`docs/SIGHTGLASS.md`)
once PR 2 lands.

## 11. Testing

- `admin-api-types`: snapshot tests locking wire shapes against the
  current `json!` output (regression gate for the extraction).
- Sightglass unit tests: state reducers and keymap dispatch (pure
  functions — model/update separated from draw for exactly this).
  Reducer fixtures are multi-node from the start: id collisions across
  nodes (same call_id on two nodes stays two rows), action routing to
  the owning node, one-node-down merge behavior.
- Rendering: `ratatui::backend::TestBackend` golden tests for the main
  screens at two terminal sizes, including a fleet view with one node
  down and the single-node layout (Node column auto-hidden).
- Daemon endpoints: same in-crate dispatch-test pattern already used
  in `admin.rs` (stub registries, assert status + body).
- Manual gate before each PR: run against a local daemon with the
  §5.3 smoke-test stack; kill a live sipp call from the TUI.
