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

> **Inbound UAS only.** SiphonAI v0.1.0 is a UAS — it terminates
> inbound TLS connections and writes responses back over the
> established socket. Originating a new outbound TLS connection
> (UAC mode) is not supported and will return a clear error to the
> transaction manager rather than silently downgrading to UDP.

### `[sip.call_progress]`

What — if any — provisional response the UAS layers on top of
`IntegratedUAS`'s `100 Trying` before the 2xx. Defaults to
`instant_answer` (v0.1.0 behaviour).

| Field  | Type   | Default          | Notes |
|--------|--------|------------------|-------|
| `mode` | string | `instant_answer` | One of `"instant_answer"`, `"ringing"`, `"session_progress"`. Anything else is rejected at load. |

```toml
[sip.call_progress]
mode = "session_progress"
```

| Mode                | Wire behaviour                                                                     | Use case                                                            |
|---------------------|------------------------------------------------------------------------------------|---------------------------------------------------------------------|
| `instant_answer`    | No extra provisional; `100 Trying` then straight to `200 OK`.                       | AI receptionists, voice bots, demos.                                |
| `ringing`           | `180 Ringing` (no body) before `200 OK`.                                            | PBX-style call flows that expect ringback signalling.               |
| `session_progress`  | `183 Session Progress` carrying the negotiated answer SDP, then `200 OK`.           | Carrier-style integrations that route or bill on early-media SDP.   |

> **`session_progress` and `100rel`.** SiphonAI 0.2.0 sends 183
> best-effort (no `Require: 100rel`). When the inbound INVITE
> *requires* `100rel`, the call falls back to `instant_answer` for
> that call with a `warn!` log, rather than sending a non-compliant
> unreliable 183 to a peer that demanded reliable provisionals. The
> reliable-provisional path is targeted at 0.2.1 / 0.3.0 alongside
> `BridgeIn::Answer` (the "AI plays during 183 phase" flow).

## `[media]`

| Field                       | Type             | Default                | Notes |
|-----------------------------|------------------|------------------------|-------|
| `codecs`                    | `["pcmu","pcma","g722"]` | `["pcmu","pcma"]` | Priority-ordered. Opus is rejected at load — its 48 kHz audio rate isn't supported on the WS path yet. |
| `dtmf`                      | `"rfc2833" \| "off"` | `"rfc2833"`      | `"off"` disables the `telephone-event` payload type. |
| `rtp_port_range`            | `[min, max]`     | forge default          | Both ports must be even; min < max. |
| `inactivity_timeout_secs`   | integer          | `60`                   | Tear the call down after this many seconds with no inbound RTP. `0` disables the watchdog. |
| `srtp`                      | `"off" \| "preferred" \| "required"` | `"off"` | SRTP negotiation mode. `"off"` answers plaintext-only and rejects SRTP offers with 488. `"preferred"` answers SRTP when offered, plaintext otherwise. `"required"` rejects plaintext-RTP offers with 488. Per-route override via `[route.media].srtp`. **Wire behaviour ships across Sprint 1 Weeks 2 / 3 of the 0.3.0 plan; the config surface exists from W1 so per-route merge logic and the `start.srtp` event field have stable types to bind to. Setting any value other than `"off"` before W3 has no effect.** |

> **SRTP over plaintext SIP is a footgun.** SDES exchanges the
> SRTP master key over the signalling plane — if `[sip]` is plain
> UDP, the key travels in cleartext and SRTP gives you nothing.
> Pair `[media].srtp = "preferred"` or `"required"` with
> `[sip.tls]`. The config-load step warns when SRTP is enabled
> but no TLS listener is bound (Sprint 1 Week 1 of the 0.3.0
> plan adds the warning).

## `[bridge]`

| Field                      | Type      | Default  | Notes |
|----------------------------|-----------|----------|-------|
| `ws_url`                   | URL       | unset    | If unset, every route MUST set its own `ws_url` or the call rejects with 503. |
| `ws_auth_header`           | string    | unset    | Sent verbatim as the `Authorization` header. `${VAR}` expansion works. |
| `ws_connect_timeout_ms`    | integer   | `5000`   | WS handshake budget. |
| `forward_headers`          | string[]  | `[]`     | SIP header names (case-insensitive) to copy onto `start.sip.headers`. |
| `silence_threshold_ms`     | integer   | `3000`   | One-sided: emit `silence_detected` (PROTOCOL §3.6) when the caller has been VAD-silent for this long. `0` disables. Per-route override via `[route.bridge].silence_threshold_ms`. |
| `dead_air_threshold_ms`    | integer   | `10000`  | Two-sided: emit `dead_air_detected` (PROTOCOL §3.7) when neither caller speech nor outbound WS audio has been observed for this long. `0` disables. Per-route override via `[route.bridge].dead_air_threshold_ms`. |
| `rtp_stats_interval_ms`    | integer   | `5000`   | Cadence of periodic `rtp_stats` events (PROTOCOL §3.8). Default mirrors RTCP §6.2's compound-report cadence so emissions align with the underlying RTCP arrivals. `0` disables the event entirely. Per-route override via `[route.bridge].rtp_stats_interval_ms`. |

> **Detection cadence.** Silence / dead-air events are polled every
> 500 ms, so the `duration_ms` on the wire may overshoot the
> configured threshold by up to that amount. Acceptable for the
> "are you still there?" / "hang up the dead call" use cases these
> primitives target; sub-second accuracy needs a different design.
> `rtp_stats` emissions hit the configured interval exactly (no
> coarsening).

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

## `[[trunk]]` — peer-trunk allowlist

Identifies inbound SIP peers (other PBXes / carriers) by source IP
and/or `From:` URI host. Acts as a 403 gate at the SIP layer so
the daemon doesn't have to rely on a firewall in front of it for
trust decisions.

| Field        | Type     | Default      | Notes |
|--------------|----------|--------------|-------|
| `name`       | string   | required, unique | Dialplan handle. Becomes the call's `register_source` when this trunk matches. |
| `peer_addrs` | string[] | unset        | Allowed source addresses. Each entry is either an exact IP (`"203.0.113.10"`) or a CIDR (`"10.0.0.0/24"`, `"2001:db8::/32"`). Exact IPs are stored as `/32` (IPv4) or `/128` (IPv6). |
| `from_hosts` | string[] | unset        | Allowed `From:` URI hostnames, case-insensitive. For trunks whose egress IP rotates but the SIP From domain is stable. |

```toml
[[trunk]]
name       = "freeswitch-main"
peer_addrs = ["10.0.0.10"]

[[trunk]]
name       = "lab-pbx"
peer_addrs = ["10.1.0.0/24"]

[[trunk]]
name       = "carrier-b"
from_hosts = ["sip.carrier-b.example"]   # rotating IPs; pin by domain

[[route]]
name = "fs-9000"
[route.match]
register_source = "freeswitch-main"      # scope route to this trunk
request_uri_user = "9000"
```

### Semantics

- A trunk MUST declare at least one of `peer_addrs` / `from_hosts`.
  A trunk with neither is refused at config load (an empty trunk
  would accept anything claiming its name).
- When both fields are populated, **both must match** (defense
  in depth). For an OR relationship, declare two `[[trunk]]`
  blocks.
- Trunks are walked in declaration order; first match wins.
- **Zero `[[trunk]]` blocks defined**: the daemon stays in legacy
  "accept any source" mode. `register_source` defaults to
  `"trunk"` for unregistered inbound (matching today's behavior).
  Documented as **dev / behind-firewall only** — production
  deployments should declare trunks.
- **One or more `[[trunk]]` blocks defined**: an INVITE that
  matches no trunk is rejected with `403 Forbidden` at the
  routing layer, before any media setup or per-call task runs.
  Logged at WARN with peer IP for diagnostics.

### Threat model

- `peer_addrs` is strong against off-path attackers. Weak against
  on-path attackers (anyone who can spoof IPv4 source addresses on
  your network) and against attackers who can reach you from any
  IP inside the allowlisted CIDR.
- `from_hosts` is a shared-secret-in-a-header pattern. Any peer
  that can reach you on UDP 5060 can forge `From: <sip:x@your-host>`.
  Useful as a *second factor* alongside `peer_addrs`; marginally
  useful alone if a trusted upstream firewall validates From
  rewrites.
- The strong combination is `peer_addrs` + `from_hosts` + TLS
  transport. Internet-facing deployments with `from_hosts`-only
  trunks should pin TLS at the transport (`transports = ["tls"]`
  with the carrier's cert pinned by the OS trust store).
- Digest auth on inbound INVITEs (RFC 3261 §22) is the proper
  "no trust in network" answer; it's a post-v1 feature.

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
