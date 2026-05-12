# SiphonAI Dialplan

How SiphonAI decides which WebSocket server an inbound call should
bridge to. The dialplan lives in the same TOML file as the rest of
the daemon's configuration, as a sequence of `[[route]]` entries.

This document is the canonical grammar reference. If something here
disagrees with `crates/routes/`, the code is wrong — file an issue.

---

## 1. Model

```
inbound INVITE ─► extract CallInfo ─► first matching [[route]] ─► bridge config
                                              │
                                              └─► no match → SIP 404
```

One INVITE, one route. Routes never combine; whichever matches first
wins, and the call uses that route's bridge config (merged over the
global `[bridge]` and `[media]` defaults).

---

## 2. Where Routes Live

```toml
[[route]]
name = "main_reception"
[route.match]
request_uri_user = "5000"
[route.bridge]
ws_url = "wss://reception.example.com/sip-bridge"

[[route]]
name = "default"
[route.match]
any = true
[route.bridge]
ws_url = "wss://default.example.com/sip-bridge"
```

Each `[[route]]` has:

- `name` — non-empty, unique across the file. Surfaces in logs,
  metrics labels, and HEP correlation.
- `[route.match]` — predicate(s) the call must satisfy.
- `[route.bridge]` *(optional)* — overrides for the global
  `[bridge]` block. Anything not specified inherits.
- `[route.media]` *(optional)* — overrides for the global `[media]`
  block. Same merge rules.

---

## 3. Evaluation Rules

These rules are CLAUDE.md §4.6 cardinal — don't try to bend them:

1. **Order matters.** Routes evaluate top to bottom. The first
   route whose `[route.match]` predicates *all* hold wins. The order
   of routes in the file *is* the priority — there are no priority
   numbers, scores, or weighted matching.
2. **AND across keys within a route.** All match keys must hold.
   For OR semantics, write multiple routes.
3. **Per-route override.** Set fields in `[route.bridge]` or
   `[route.media]` override the corresponding global fields *for
   that call only*. Unset fields inherit from globals — a route
   never silently ignores a global setting.
4. **No match → SIP 404.** If you want a catch-all, write a default
   route at the end with `any = true`. Production deployments
   without a default get a startup warning.
5. **Validation is at config load time.** Invalid regex, conflicting
   keys, duplicate route names, missing register references — all
   fail loud at startup, not on the first call that would have hit
   them.

---

## 4. Match Keys

All match keys are optional. Use as many as you need; they AND.

| Key                  | Source on the inbound INVITE                                                          |
|----------------------|---------------------------------------------------------------------------------------|
| `request_uri_user`   | User-part of the Request-URI (the part before `@`).                                  |
| `request_uri_host`   | Host-part of the Request-URI.                                                         |
| `to_user`            | User-part of the `To` header URI.                                                     |
| `to_host`            | Host-part of the `To` header URI.                                                     |
| `from_user`          | User-part of the `From` header URI.                                                   |
| `from_host`          | Host-part of the `From` header URI.                                                   |
| `register_source`    | Name of the `[[register]]` block the call arrived on, or `"trunk"` for unregistered. |
| `header.<NAME>`      | Value of header `<NAME>` (case-insensitive on the name).                              |
| `any`                | Boolean. `true` matches everything; mutually exclusive with all the above.            |

### 4.1 String matching

By default every string match value is a **case-insensitive exact
match**. `request_uri_user = "5000"` matches `5000`, not `5000abc`.

### 4.2 Regex matching

Set `regex = true` on the `[route.match]` block to reinterpret every
string match value in that route as a Rust regex. The flag is
**per-route, not per-key** — you can't mix literal and regex
predicates in one route.

```toml
[[route]]
name = "sales-reps"
[route.match]
regex = true
request_uri_user = "^sales-[0-9]+$"   # whole-string with anchors
from_host = "carrier\\."              # substring; carrier.anything
```

Regex anchoring is up to you: `^foo$` is whole-string, `foo` is
substring. This matches what users expect from `grep`/`ripgrep`. If
the regex doesn't compile, config load fails.

### 4.3 Header matching

```toml
[[route]]
name = "by-customer"
[route.match]
regex = true
[route.match.header]
X-Customer-Id = "^cust-.*$"
```

Header names are case-insensitive on lookup. Header values follow
the same literal-or-regex rule as other string keys, governed by the
route's `regex` flag. Multiple `header.*` predicates AND. A header
that's absent from the INVITE matches as the empty string — usually
that means no match (a regex like `^$` would match, but that's the
user's choice).

### 4.4 `register_source`

`register_source` matches the `name` field of the `[[register]]`
block the call arrived through, or the literal `"trunk"` for inbound
calls that hit the SIP listener directly without traversing a
registration. Useful for routing CUCM-originated calls separately
from Asterisk-originated calls when SiphonAI registers to multiple
PBXes.

```toml
[[register]]
name = "cucm-main"
# ... cucm config

[[route]]
name = "from_cucm"
[route.match]
register_source = "cucm-main"
[route.bridge]
ws_url = "wss://cucm-handler.example.com/sip-bridge"
```

Validation rejects routes referencing a `register_source` name that
doesn't exist (this check lives in the config crate, which sees both
sides; the routes crate alone matches whatever string the daemon
hands it).

### 4.5 `any`

```toml
[[route]]
name = "default"
[route.match]
any = true
```

Unconditional match. Place it at the end of the file as the
catch-all. `any = true` together with any other match key is a
config-load error — silent precedence between "match anything" and
"match this specific thing" would be a footgun.

---

## 5. Worked Examples

### 5.1 Three reception lines + default

```toml
[[route]]
name = "english_line"
[route.match]
request_uri_user = "5000"
[route.bridge]
ws_url = "wss://en.example.com/sip-bridge"

[[route]]
name = "french_line"
[route.match]
request_uri_user = "5001"
[route.bridge]
ws_url = "wss://fr.example.com/sip-bridge"

[[route]]
name = "spanish_line"
[route.match]
request_uri_user = "5002"
[route.bridge]
ws_url = "wss://es.example.com/sip-bridge"

[[route]]
name = "default"
[route.match]
any = true
[route.bridge]
ws_url = "wss://default.example.com/sip-bridge"
```

### 5.2 VIP routing by caller ID, fallback by department

```toml
# Highest priority: a VIP, regardless of which line they dialed.
[[route]]
name = "vip"
[route.match]
from_user = "+13125551234"
[route.bridge]
ws_url = "wss://vip.example.com/sip-bridge"

# Department dispatch on Request-URI user, with regex.
[[route]]
name = "sales"
[route.match]
regex = true
request_uri_user = "^sales-[0-9]+$"
[route.bridge]
ws_url = "wss://sales.example.com/sip-bridge"

[[route]]
name = "support"
[route.match]
regex = true
request_uri_user = "^support-[0-9]+$"
[route.bridge]
ws_url = "wss://support.example.com/sip-bridge"

[[route]]
name = "default"
[route.match]
any = true
[route.bridge]
ws_url = "wss://reception.example.com/sip-bridge"
```

The VIP route wins for that caller even if they dial a sales line —
order in the file is the priority.

### 5.3 Per-tenant header from upstream PBX

```toml
[[route]]
name = "tenant_acme"
[route.match]
regex = true
[route.match.header]
X-Tenant-Id = "^acme$"
[route.bridge]
ws_url = "wss://acme.bridges.example.com/sip-bridge"
ws_auth_header = "Bearer ${BRIDGE_TOKEN_ACME}"

[[route]]
name = "tenant_globex"
[route.match]
regex = true
[route.match.header]
X-Tenant-Id = "^globex$"
[route.bridge]
ws_url = "wss://globex.bridges.example.com/sip-bridge"
ws_auth_header = "Bearer ${BRIDGE_TOKEN_GLOBEX}"

[[route]]
name = "default"
[route.match]
any = true
[route.bridge]
ws_url = "wss://shared.example.com/sip-bridge"
```

### 5.4 Mode-aware routing

```toml
# Calls that came in via the registered AOR on Asterisk get one
# handler; calls that hit the trunk listener get another.
[[route]]
name = "from_asterisk"
[route.match]
register_source = "asterisk-sales"
[route.bridge]
ws_url = "wss://internal.example.com/sip-bridge"

[[route]]
name = "trunk_inbound"
[route.match]
register_source = "trunk"
[route.bridge]
ws_url = "wss://external.example.com/sip-bridge"

[[route]]
name = "default"
[route.match]
any = true
[route.bridge]
ws_url = "wss://shared.example.com/sip-bridge"
```

---

## 6. Common Mistakes

- **Default route in the middle.** Anything below it is unreachable.
  The daemon doesn't enforce trailing position (you might want it
  while testing) but a startup warning fires whenever the *last*
  route isn't the default.
- **Mixing literal and regex in one route.** Not supported — split
  into two routes.
- **Regex without anchors.** `request_uri_user = "5000"` with
  `regex = true` matches `5000`, `15000`, `50001` — substring, not
  whole-string. If you want exact, use `^5000$`.
- **Quoted dotted header keys.** `"header.X-Customer-Id" = "..."`
  in TOML produces a single literal key with a dot; that's not a
  header predicate. Use the nested table form (`[route.match.header]`)
  or unquoted dotted keys (`header.X-Customer-Id = "..."`).
- **Duplicate route names.** Names are how observability
  correlates calls; collisions are a hard error at load time.

---

## 7. See Also

- `docs/DEV_PLAN.md` §6 — full TOML schema, including registrations,
  HEP, CDRs, and how route overrides merge against globals.
- `docs/PROTOCOL.md` — what the matched route's `bridge.ws_url`
  receives.
- `crates/routes/` — the implementation. The `RouteSet::find_match`
  function is the entry point.
