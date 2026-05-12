# Configuration Reference

SiphonAI is configured by a single TOML file. The path is supplied with `--config`
on the daemon binary. TOML is the only supported format (CLAUDE.md §4.6); all
validation runs at config load time, not first-use, so a bad config fails
loudly at startup instead of mid-call.

`${VAR}` and `${VAR:-default}` are expanded from the process environment
before TOML parsing. Unset variables without a default fail the load.

## Top-level layout

```toml
[node]            # daemon identity
[sip]             # SIP transport
[sip.tls]         # optional TLS leaf
[media]           # codecs / DTMF / RTP / inactivity watchdog
[bridge]          # WS defaults (per-route can override)
[bridge.barge_in] # speech-detection policy
[[route]]         # one per dialplan rule, ORDERED (first match wins)
[[register]]      # zero or more outbound REGISTERs ("registered phone" mode)
[cdr]             # call detail records: file + webhook sinks
[observability]   # /metrics, /health, /ready, /admin
[webhooks]        # lifecycle events (call_start, call_end, …)
[hep]             # HEP3 shipping to Homer
```

## `[node]`

| Field            | Type   | Default        | Notes |
|------------------|--------|----------------|-------|
| `id`             | string | `"siphon-ai"`  | Appears in logs, metrics labels, HEP capture metadata. |
| `public_address` | string | `[sip].listen` IP | Required when `[sip].listen` binds the wildcard (`0.0.0.0` / `::`) — the SDP `c=` line can't advertise an unspecified address. |

## `[sip]`

| Field        | Type             | Default    | Notes |
|--------------|------------------|------------|-------|
| `listen`     | `host:port`      | required   | UDP/TCP bind. UDP and TCP share this port. |
| `transports` | `["udp","tcp","tls"]` | `["udp"]` | Subset enabled. `"tls"` requires `[sip.tls]`. |
| `user_agent` | string           | unset      | Set to brand the `User-Agent` and `Server` headers. |
| `contact`    | string           | derived    | Override the `Contact` URI; otherwise built from `[node].public_address` + the bound port. |

### `[sip.tls]`

| Field    | Type        | Default              | Notes |
|----------|-------------|----------------------|-------|
| `listen` | `host:port` | same IP + port `5061` | Where the TLS listener binds. |
| `cert`   | path        | required when TLS on | PEM cert chain on disk. |
| `key`    | path        | required when TLS on | PEM private key on disk. |

## `[media]`

| Field                       | Type             | Default                | Notes |
|-----------------------------|------------------|------------------------|-------|
| `codecs`                    | `["pcmu","pcma","g722"]` | `["pcmu","pcma"]` | Priority-ordered. Opus is rejected at load — its 48 kHz audio rate isn't supported on the WS path yet. |
| `dtmf`                      | `"rfc2833" \| "off"` | `"rfc2833"`      | `"off"` disables the `telephone-event` payload type. |
| `rtp_port_range`            | `[min, max]`     | forge default          | Both ports must be even; min < max. |
| `inactivity_timeout_secs`   | integer          | `60`                   | Tear the call down after this many seconds with no inbound RTP. `0` disables the watchdog. |

## `[bridge]`

| Field                    | Type      | Default | Notes |
|--------------------------|-----------|---------|-------|
| `ws_url`                 | URL       | unset   | If unset, every route MUST set its own `ws_url` or the call rejects with 503. |
| `ws_auth_header`         | string    | unset   | Sent verbatim as the `Authorization` header. `${VAR}` expansion works. |
| `ws_connect_timeout_ms`  | integer   | `5000`  | WS handshake budget. |
| `forward_headers`        | string[]  | `[]`    | SIP header names (case-insensitive) to copy onto `start.sip.headers`. |

### `[bridge.barge_in]`

| Field         | Type   | Default       | Notes |
|---------------|--------|---------------|-------|
| `enabled`     | bool   | `true`        | When `false`, VAD events still flow but `mode` degrades to `notify_only`. |
| `mode`        | `"auto_clear" \| "notify_only"` | `"auto_clear"` | `auto_clear` drops pending playout the moment forge-vad reports speech. |
| `debounce_ms` | integer | `100`        | Reserved for the VAD config — currently informational. |

## `[[route]]`

Routes are ORDERED. The first one whose `match` block evaluates true wins.
Add a trailing `any = true` route as the default, or unmatched INVITEs get 404.
See `docs/DIALPLAN.md` for the full match grammar.

```toml
[[route]]
name = "main_reception"          # unique; appears in logs + metrics labels
[route.match]
request_uri_user = "5000"        # all keys AND together within one route
regex = false                    # per-route flag; on means every string value is a regex
[route.bridge]
ws_url = "wss://reception.example.com/sip-bridge"
ws_auth_header = "Bearer ${BRIDGE_TOKEN_RECEPTION}"
on_ws_failure = "hangup"         # v1 only supports "hangup"
[route.media]
codecs = ["pcma", "pcmu"]        # override the global priority for this route
dtmf = "off"
inactivity_timeout_secs = 30     # override [media].inactivity_timeout_secs
```

Match keys (any combination, all AND together): `request_uri_user`,
`request_uri_host`, `to_user`, `to_host`, `from_user`, `from_host`,
`register_source`, `header.<NAME> = "<value>"`, `any = true`.

## `[[register]]` — registered-phone mode

Zero or more allowed. Each block becomes a `register_source` key visible to
the dialplan (`[route.match].register_source = "cucm-main"`). The daemon
sends REGISTER on startup, refreshes at `expires - 60s`, and retries with
exponential backoff (5s → 5 min cap) on failure.

| Field                  | Type    | Default                 | Notes |
|------------------------|---------|-------------------------|-------|
| `name`                 | string  | required, unique        | The dialplan handle. |
| `server`               | host or host:port | required        | Registrar. `port` overrides any port here. |
| `port`                 | integer | 5060 (udp/tcp), 5061 (tls) | |
| `transport`            | `"udp"` \| `"tcp"` \| `"tls"` | `"udp"` | TLS uses the daemon's client trust roots. |
| `username`             | string  | required                | SIP From username + AOR (`sip:<username>@<server>`). |
| `auth_username`        | string  | `username`              | Digest challenge response identity. |
| `password`             | string  | required                | `${VAR}` env-expanded. Don't commit this. |
| `realm`                | string  | unset                   | Mostly informational — registrar's challenge wins. |
| `expires_secs`         | integer | `3600`                  | Registration lifetime. |
| `register_on_startup`  | bool    | `true`                  | `false` keeps the block configured-but-idle (useful for incident response). |

## `[cdr]`

```toml
[cdr]
enabled = true

[cdr.file]                    # JSONL, one record per ended call
enabled = true
path    = "/var/log/siphon-ai/cdr.jsonl"

[cdr.webhook]                 # HTTP POST per record
enabled    = true
url        = "https://billing.example.com/cdr"
auth_header = "Bearer ${CDR_TOKEN}"
retry_max  = 3
timeout_ms = 5000
```

Both sinks can run simultaneously. Master `enabled = false` silences both
regardless of sub-block state. Adding fields to the CDR schema bumps the
`version` field; consumers should treat new keys as additive.

## `[observability]`

| Field         | Type        | Default        | Notes |
|---------------|-------------|----------------|-------|
| `enabled`     | bool        | `false`        | Master switch for the HTTP server. |
| `http_listen` | `host:port` | required if on | Exposes `/metrics`, `/health`, `/ready`, `/admin/*`. |

## `[webhooks]`

| Field         | Type     | Default                | Notes |
|---------------|----------|------------------------|-------|
| `enabled`     | bool     | `false`                | |
| `url`         | URL      | required if on         | POST target. |
| `auth_header` | string   | unset                  | Sent verbatim. `${VAR}` expansion works. |
| `events`      | string[] | all                    | Allowlist. Valid today: `"call_start"`, `"call_end"`, `"registration_state_changed"`. |
| `retry_max`   | integer  | `3`                    | |
| `timeout_ms`  | integer  | `5000`                 | |

## `[hep]`

```toml
[hep]
enabled          = true
collector        = "homer.example.com:9060"   # UDP only in v1
capture_id       = 2001
capture_password = "${HEP_PASSWORD}"
queue_capacity   = 256                        # default
```

When `enabled = true`, `collector` and `capture_id` are required. HEP
emission is best-effort: a full queue drops with a metric tick, and an
unreachable collector flips `siphon_ai_hep_collector_up` to 0. The audio
path never blocks on HEP (CLAUDE.md §4.7). See `docs/HEP.md` for what each
layer ships and how Homer correlates them.

## Reload

The daemon does not currently support config reload — changes take effect on
process restart. Routing changes that operators want to apply mid-shift
should be made via the admin API (`POST /admin/calls/:id/hangup`,
`PUT /admin/log`) rather than by reloading.
