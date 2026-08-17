# Sightglass — terminal operator console

A sight glass is the fitting on a pipe that lets you watch the fluid
moving through it. `sightglass` is that for one or more running
siphon-ai nodes: a ratatui terminal application that renders each
node's admin API as a live, tabbed console — and, with an operator
token, acts on it: hang up a call, park and retrieve, originate,
move a call into a conference.

It is a **pure admin-API client**. It speaks only to the `[admin]`
HTTP listener, holds no SIP or RTP, and can run anywhere that can
reach that listener. Design rationale and build plan:
`design/DESIGN_SIGHTGLASS.md`.

---

## Prerequisites

1. **The `[admin]` listener must be enabled** on every node you want
   to watch. Omitted ⇒ `/admin/*` is not served at all, and sightglass
   shows the node as down. Minimum config:

   ```toml
   [admin]
   listen = "127.0.0.1:9092"

   [[admin.token]]
   name  = "noc-wall"
   token = "${SIGHTGLASS_TOKEN_PROD1}"
   role  = "readonly"          # readonly | operator | admin
   ```

   See `CONFIG.md` → `[admin]` and `DEPLOY.md` → *Admin auth & RBAC*.

2. On a routable bind, serve the listener over `[admin.tls]` (or front
   it with a TLS proxy) — the bearer token is otherwise plaintext on
   the wire. Sightglass supports a per-node private CA (`ca = …`).

## Running

Ad-hoc, one node:

```bash
sightglass --target https://prod-1.example.com:9092 --token-file ~/.config/sightglass/prod-1.token
```

A fleet, via `~/.config/sightglass/config.toml`:

```toml
poll_interval_ms = 1000   # per node

[[node]]
name = "prod-1"
url = "https://prod-1.example.com:9090"
token_file = "prod-1.token"          # relative to this file's directory
# ca = "private-ca.pem"              # if the admin TLS cert is privately signed

[[node]]
name = "prod-2"
url = "https://prod-2.example.com:9090"
token_file = "prod-2.token"
```

`node.name` must be unique — it is the identity shown in every Node
column and confirm modal. Tokens come from files (or
`$SIGHTGLASS_TOKEN` for single-node `--target` use), never CLI
arguments — argv is visible in `ps`.

Flags: `--read-only` disables every mutating action client-side
regardless of token role (for NOC wall screens); `--ascii` swaps the
Unicode status glyphs for ASCII.

## Tabs and keys

| Key | Effect |
|---|---|
| `1` / `2` / `3`, `⇥` / `⇧⇥` | switch tab (overview / trunks / calls) |
| `n` | cycle the node filter (all → node → … → all); scopes every tab |
| `j`/`k`, `↓`/`↑`, `g`/`G` | move the call selection |
| `q`, `Esc`, `Ctrl-C` | quit |

- **overview** — fleet health grid (one row per node: reachability,
  active calls, registrations up, drain state) plus a fleet-wide
  active-calls sparkline. A down node shows `○ down (retrying)` with
  its last-seen data dimmed; it never breaks the rest of the view.
- **trunks** — every `[[register]]` binding across the fleet with
  state, expiry, and last error.
- **calls** — fleet-unified call table (both id namespaces +
  direction; the Node column hides itself on single-node fleets), and
  a detail pane for the focused call: live MOS, jitter, packet
  counters, first-audio latency, barge-ins, and a MOS trend sparkline.
  Stats are polled for the focused call only.

## Actions (calls tab)

| Key | Action | Endpoint | Min role |
|---|---|---|---|
| `x` | hang up the focused call | `POST /admin/v1/calls/{id}/hangup` | operator |
| `p` | park the focused call | `POST …/park` | operator |
| `u` | retrieve — optional new `ws_url` (move a live call to a different WS server) | `POST …/retrieve` | operator |
| `c` | add the focused call to a conference room | `POST /admin/v1/conferences/{room}/participants` | operator |
| `o` | originate an outbound call (dial form) | `POST /admin/v1/calls` | admin (billable) |

Every action targets exactly one node, and the confirm modal names it
("hangup abc-123 **on prod-2**?") — on a fleet you always know which
box you're touching. Results arrive as transient toasts ("hangup …
accepted (200)"); failures show the daemon's error text. All actions
land in the daemon's audit stream attributed to your token's name.

### Role-aware keybinds

At startup sightglass learns each node's token role and greys out the
action keys that role can't use (a greyed hint reads `hangup✗`). The
403 response carries no role information, so the role is learned by
probing the RBAC gate itself — two POSTs that cannot have side
effects:

1. `POST /admin/v1/calls/sightglass-role-probe/hangup` — 403 means
   readonly; a 404 (the sentinel id never exists) means ≥ operator.
2. `POST /admin/v1/calls` with an empty body — 403 means operator; a
   validation 400 (or 501 when outbound is disabled) means admin.
   Validation rejects the empty body before anything is dialed.

You will see these two requests in the audit log at session start,
attributed to the sightglass token. `--read-only` skips the probes
entirely. If a probe can't run (node down at start), keys stay
enabled and a real 403 both toasts and greys the keys from then on.

## Troubleshooting

- **Node shows `down (retrying)` but the daemon is up** — is `[admin]`
  configured on that node? Is the URL the *admin* listener (not the
  `[observability]` one)? Wrong or missing token shows the same way
  (the error text names the status).
- **Everything is greyed** — your token's role is `readonly`, or the
  binary was started with `--read-only` (the header shows a
  `read-only` badge).
- **`origination not enabled`** — the target node has
  `[outbound].max_concurrent = 0`; enable outbound on that node
  (`CONFIG.md` → `[outbound]`).
