# Configuration Reference

SiphonAI is configured by a single TOML file. The path is supplied with `--config`
on the daemon binary. TOML is the only supported format (CLAUDE.md §4.6); all
validation runs at config load time, not first-use, so a bad config fails
loudly at startup instead of mid-call.

## Secrets & variable expansion

`${...}` references in any string value are expanded before TOML parsing, in a
single fail-loud pass — an unresolvable reference fails the load instead of a
half-substituted string reaching a parser or a call. Three source forms are
recognised:

| Form | Resolves to | Use for |
|---|---|---|
| `${VAR}` / `${VAR:-default}` | a **process environment** variable (with optional default) | the default; simplest |
| `${file:/path/to/secret}` | the **file's contents** (trailing CR/LF trimmed) | Docker / Kubernetes secrets, Vault-Agent templated files |
| `${cred:NAME}` | the contents of `$CREDENTIALS_DIRECTORY/NAME` | systemd `LoadCredential=` / `ImportCredential=` |

The `file:` and `cred:` forms (v0.18.0) let you keep secrets — admin tokens,
SIP digest passwords, webhook/CDR HMAC secrets, the HEP password — **out of the
process environment** entirely (where they'd otherwise be visible in
`/proc/<pid>/environ`, core dumps, and supervisor unit files). They work
anywhere `${VAR}` works.

- **`${file:PATH}`** reads the whole file and trims trailing newlines (so a
  secret written with `echo "x" > f` resolves to `x`, not `x\n`); leading and
  internal bytes are preserved. A missing/unreadable file fails the load.
- **`${cred:NAME}`** reads `$CREDENTIALS_DIRECTORY/NAME`. `NAME` is a flat
  identifier (no `/` or `..`). Used with systemd:

  ```ini
  # siphon-ai.service
  [Service]
  LoadCredential=admin_token:/etc/siphon-ai/secrets/admin_token
  ```
  ```toml
  # config.toml
  [[admin.token]]
  name  = "ops"
  token = "${cred:admin_token}"
  role  = "operator"
  ```
  Without `$CREDENTIALS_DIRECTORY` set, a `${cred:...}` reference fails the
  load (you didn't start under systemd `LoadCredential=`).

> **Disambiguation.** The `:-` default operator is always an env reference, so
> `${file:-x}` means "env var `file`, default `x`", **not** a `file:` lookup.
> Reference a file as `${file:/abs/path}` (a real path, no `:-`).

Unset env variables without a default fail the load; so do unreadable files and
credentials. Resolved secret values are never logged.

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
[security]        # STIR/SHAKEN call-authentication policy (0.4.0)
[security.stir_shaken]  # verification settings
[conference]      # conference rooms (0.7.0); off by default
[park]            # media-only call park (0.7.0); off by default
[cdr]             # call detail records: file + webhook sinks
[observability]   # /metrics, /health, /ready
[admin]           # authenticated /admin/* listener (0.10.0); off → /admin not served
[webhooks]        # lifecycle events (call_start, call_end, …)
[audit]           # signed audit-event stream for SIEM (0.20.0); off by default
[quality]         # per-call quality history records (0.31.0); off by default
[hep]             # HEP3 shipping to Homer
[shutdown]        # graceful drain on SIGTERM/SIGINT (0.17.0)
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
| `allow_delayed_offer` | bool    | `true`     | Accept an inbound INVITE with **no SDP** (RFC 3264 delayed offer): SiphonAI offers in the 200 OK and reads the peer's answer from the ACK. Needed for CUCM trunks/phones without a forced MTP. `false` rejects an offerless INVITE with `488`. Early-offer INVITEs are unaffected either way. |
| `tcp_idle_timeout_secs` | integer | `1800`  | Idle timeout for an **established** inbound SIP-over-**TCP/TLS** connection — one that has completed at least one SIP message. A SIP trunk (e.g. CUCM) holds this connection open for a call's whole life while sending **no SIP** (RTP is out-of-band), so this must exceed your longest SIP-quiet period, or the connection is reaped mid-call and in-dialog re-INVITEs/BYEs are lost (wedging the trunk → `503` on new calls). Default `1800` (matches common session timers). `0` disables the idle close. Does **not** shorten the short Slowloris window for connections that never complete a request. **UDP is connectionless and unaffected.** |

### `[sip.tls]`

| Field    | Type        | Default              | Notes |
|----------|-------------|----------------------|-------|
| `listen` | `host:port` | same IP + port `5061` | Where the TLS listener binds. |
| `cert`   | path        | required when TLS on | PEM cert chain on disk. |
| `key`    | path        | required when TLS on | PEM private key on disk. |

> **Server side only.** `[sip.tls]` is the inbound listener's
> cert/key. *Outgoing* TLS connections — `[[gateway]]` and
> `[[register]]` blocks with `transport = "tls"` — verify the peer
> against the client roots below and need no `[sip.tls]` at all: a
> UDP-only daemon can still dial a TLS trunk.

### `[sip.tls_client]` (0.6.2+)

| Field      | Type | Default | Notes |
|------------|------|---------|-------|
| `extra_ca` | path | unset   | PEM bundle appended to the built-in webpki (Mozilla CA) roots when verifying outgoing TLS. For trunks fronted by a private CA and for test rigs with self-signed certs. Public trunks (e.g. Twilio) verify against the built-in roots without this. |

The path is checked at config load; an unreadable or empty bundle
fails at startup, not at first dial-out.

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
| `codecs`                    | `["pcmu","pcma","g722","opus"]` | `["pcmu","pcma"]` | Priority-ordered. **`opus`** (0.8.0) negotiates `opus/48000/2` on the wire but runs at a 16 kHz bridge rate, so an Opus call surfaces as a 16 kHz session (`start.audio.sample_rate = 16000`) — forge decodes/encodes Opus at 16 kHz mono and libopus handles the 48↔16 resample + stereo→mono. The SDP advertises an Opus `a=fmtp` (0.8.2: `stereo=0; maxplaybackrate=16000; useinbandfec=1; usedtx=0` …) telling the peer to send mono at ≤16 kHz. Requires the daemon built with Opus support (libopus; see `docs/DEPLOY.md`). Not in the default list — add `"opus"` to enable. |
| `dtmf`                      | `"rfc2833" \| "off"` | `"rfc2833"`      | `"off"` disables the `telephone-event` payload type. |
| `rtp_port_range`            | `[min, max]`     | forge default          | Both ports must be even; min < max. |
| `inactivity_timeout_secs`   | integer          | `60`                   | Tear the call down after this many seconds with no inbound RTP. `0` disables the watchdog. |
| `srtp`                      | `"off" \| "preferred" \| "required"` | `"off"` | SRTP negotiation mode. `"off"` answers plaintext-only and rejects SRTP offers with 488. `"preferred"` answers SRTP when offered, plaintext otherwise. `"required"` rejects plaintext-RTP offers with 488. Per-route override via `[route.media].srtp`. **Wire behaviour ships across Sprint 1 Weeks 2 / 3 of the 0.3.0 plan; the config surface exists from W1 so per-route merge logic and the `start.srtp` event field have stable types to bind to. Setting any value other than `"off"` before W3 has no effect.** |
| `srtp_offer`                | `"sdes" \| "dtls"` | `"sdes"` | Which SRTP key-exchange to **offer** when SiphonAI is the *offerer* on a **delayed offer** (inbound offerless INVITE) and `srtp` is `"preferred"`/`"required"` (0.9.2 SDES / 0.9.4 DTLS). `"dtls"` offers `UDP/TLS/RTP/SAVPF` + `a=fingerprint` + `a=setup:actpass` and enables the handshake from the peer's ACK answer. Ignored on inbound *early* offer (there SiphonAI always **answers** the peer's choice — both SDES and DTLS), and on outbound origination (SDES). |
| `moh_file`                  | path             | unset                  | Hold music played to the caller during a **bot-initiated hold** (0.7.2 — the WS server sends `hold`/`resume`, see PROTOCOL.md §4.10). **Existence is checked at load** — a set-but-missing path fails startup loud. Unset → generated comfort silence. Like `[park].moh_file`, the file's native sample rate must match the call's negotiated rate; a mismatch falls back to comfort silence (no resampling). Bot-hold itself is always available on inbound legs — this field only chooses what the held caller hears, it does not enable/disable the feature. |

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
| `ws_ping_interval_secs`    | integer   | `15`     | WS keepalive ping cadence (PROTOCOL §5.6). SiphonAI pings the WS server this often; combined with `ws_pong_timeout_secs` it detects a half-open / hung server. `0` disables keepalive. Daemon-wide (not route-overridable). |
| `ws_pong_timeout_secs`     | integer   | `10`     | How long an outstanding keepalive ping may go un-ponged before the session is declared half-open (PROTOCOL §5.6). With `ws_reconnect_enabled` this triggers reconnect; otherwise teardown. `0` disables keepalive. |
| `server_start_deadline_secs` | integer | `5`     | The WS server must send its first audio frame (or a `hangup`) within this window of `start`, else the call is torn down with `error { code: "server_too_slow" }` (PROTOCOL §3.1/§3.10). Raise it for servers that legitimately need longer to first audio (cold-start LLM/TTS); `0` disables the deadline. Daemon-wide. |
| `ws_reconnect_enabled`     | bool      | `false`  | Opt-in automatic WS reconnect mid-call (0.7.3). When `true`, an **unexpected** WS drop (server closed the socket without a `hangup`, IO/TLS error, keepalive timeout) keeps the caller on hold music and re-dials the same `ws_url`, resuming on a fresh session (`start.reconnected: true`, PROTOCOL §3.1) instead of tearing the call down (PROTOCOL §5.7). **Note:** to *end* a call with this on, the server sends `hangup` — a bare socket close is treated as a drop and reconnected. Per-route override via `[route.bridge].ws_reconnect_enabled`. |
| `ws_reconnect_max_secs`    | integer   | `30`     | Total window (seconds) a call may spend reconnecting before falling back to §5.7 teardown — how long the caller hears hold music before SiphonAI gives up. Must be `> 0` when `ws_reconnect_enabled = true` (fail-loud at load). Per-route override via `[route.bridge].ws_reconnect_max_secs`. |
| `on_ws_failure`            | `"hangup" \| "play_prompt"` | `"hangup"` | What a call does when its WS becomes **unusable** (0.34.0): unexpected drop, connect failure at answer, keepalive timeout, `protocol_error`, `server_too_slow`, or an exhausted reconnect window. `hangup` = immediate teardown (the v1 behaviour). `play_prompt` = play `ws_failure_prompt_file` to the caller first (*"we're experiencing difficulties…"*), then the normal BYE teardown — CDR cause unchanged, `duration_ms` grows by the prompt. Never fires when the server *intended* the ending (`hangup`/clean `stop`), on caller actions, on `rtp_timeout`, or during drain. Previously a per-route-only key accepting `"hangup"` alone; now also the global default. Per-route override via `[route.bridge].on_ws_failure`. |
| `ws_failure_prompt_file`   | path      | unset    | WAV played by `on_ws_failure = "play_prompt"` — **bridge rate** (8/16 kHz mono, like `[media].moh_file` and the consent announcement; no resampler). Required + existence-checked at load when any effective policy is `play_prompt`; files longer than the 30 s playback cap warn at load. Per-call unusability (e.g. rate mismatch on a 16 kHz call with an 8 kHz file) **fails open** to a plain hangup — the prompt is a courtesy, not compliance. Per-route override via `[route.bridge].ws_failure_prompt_file`. |

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
| `mode`        | `"auto_clear" \| "notify_only" \| "pause"` | `"auto_clear"` | `auto_clear` drops pending playout the moment forge-vad reports speech. `pause` (0.32.0) drops it **reversibly** — the unplayed tail is retained and the WS server rules on intent via `barge_in_confirm`/`barge_in_reject` (PROTOCOL.md §3.2/§4.11); a reject resumes playout where it stopped. Announced to the server on `start.barge_in_mode`. |
| `debounce_ms` | integer | `0` (off) | **Playout-gated barge-in debounce (0.7.x).** While the bot is playing out, a speech-started is held for this many ms and only flushes if speech *sustains* past it — an echo / brief-background-noise gate. Crucially it does **not** delay barge-in while the bot is silent, so a caller interrupting between bot phrases is still instant. `0`/unset = off (immediate flush, the original behaviour). Affects `auto_clear` and `pause` (the acoustic gate runs in front of the semantic arbitration — they compose). Start around `150`–`250` if the bot is hearing its own echo or background noise as barge-in. Per-route override via `[route.bridge.barge_in].debounce_ms`. |
| `decision_ms` | integer | `500` | **Pause mode only** (rejected under any other mode; `0` rejected). How long the server has to send a verdict before `on_timeout` applies. 500 ms covers STT-partial latency for the major engines; raise it if your server's first partial routinely takes longer. Per-route override via `[route.bridge.barge_in].decision_ms`. |
| `on_timeout` | `"confirm" \| "reject"` | `"confirm"` | **Pause mode only.** Fallback verdict at the deadline. `confirm` fails toward silence (never talk over the caller) — a server that ignores arbitration entirely thereby degrades to "auto_clear delayed by `decision_ms`". `reject` fails toward resuming playout. |
| `resume_max_secs` | integer | `30` | **Pause mode only** (`0` rejected). Cap on retained (resumable) audio per call — memory ceiling ≈ `secs × sample_rate × 2` bytes. A single utterance longer than this loses its oldest unplayed frames on a reject (warned once per call). |

> **When to use `debounce_ms`.** If the bot stops talking because it hears
> its own audio echoed back (poor far-end echo cancellation) or background
> noise, a 150–250 ms debounce filters those — they're typically short
> bursts (quick speech-started → speech-stopped) that never reach the
> threshold, whereas a real interruption sustains. It's a heuristic, not
> echo cancellation: a caller who genuinely interrupts *during* bot speech
> waits up to `debounce_ms` before the bot yields. For full-duplex
> interruption with no trade-off, server-side AEC is the proper fix.

> **When to use `mode = "pause"` (0.32.0).** If false barge-ins are
> *speech-shaped* — coughs, laughs, backchannels ("uh-huh") that debounce
> can't filter — pause mode lets the layer that actually has STT decide,
> while keeping the reaction instant and reversible: the bot goes quiet
> within one frame, and a `barge_in_reject` resumes it mid-utterance. A
> false positive then costs a sub-second dip instead of a killed
> utterance. Requires a WS server that sends verdicts (SDKs ≥ 0.32.0
> expose `barge_in_confirm()` / `barge_in_reject()`); a server that
> never rules degrades safely via `on_timeout`. Arbitration is suspended
> inside conference rooms (behaves as `notify_only` there). See
> `docs/design/DESIGN_REVERSIBLE_BARGE_IN.md` for the full model.

### `[bridge.tls]` (0.3.0+)

mTLS authentication for the bridge WS leg. Present means the daemon
hands a custom rustls `ClientConfig` to `tokio-tungstenite`'s
`Connector::Rustls`, carrying the configured client cert and
(optionally) an SPKI pin replacing the default Mozilla CA bundle.
Absent means the existing plaintext / webpki path. See
[`docs/DEPLOY.md`](DEPLOY.md) §3a for the operational recipe.

| Field          | Type   | Default | Notes |
|----------------|--------|---------|-------|
| `client_cert`  | string | —       | Required. Path to a PEM file containing the client cert chain (leaf first). |
| `client_key`   | string | —       | Required. Path to a PEM file containing the private key matching `client_cert`'s leaf. PKCS#8 / RSA / SEC1 all accepted. |
| `pinned_sha256`| string | unset   | Optional. 64-hex-char SHA-256 of the **server's** SubjectPublicKeyInfo. When set, replaces default CA verification with exact-match. Survives cert rotation as long as the key pair stays stable (RFC 7469 §3). |

Per-route override (`[route.bridge.tls]`, 0.15.0) — a `[route.bridge.tls]`
block on a `[[route]]` **fully replaces** this global block for calls that
match that route (same field shape; a complete client config, not a
field-by-field merge). Routes without it inherit the global. Validated at
config load and reloaded with the route table on `SIGHUP`. See
`docs/DIALPLAN.md` §5.5.

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
on_ws_failure = "hangup"         # or "play_prompt" (0.34.0) — see [bridge].on_ws_failure
[route.media]
codecs = ["pcma", "pcmu"]        # override the global priority for this route
dtmf = "off"
inactivity_timeout_secs = 30     # override [media].inactivity_timeout_secs
[route.security]
min_attestation = "A"            # strict override of [security].min_attestation
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
| `auth_required` | bool | `false`      | Require `[sip.auth]` digest authentication for INVITEs from this trunk, **in addition** to the allowlist match (0.19.0). Leave `false` for a static-IP carrier that doesn't send credentials; set `true` for a trunk with no stable egress IP. Requires `[sip.auth].enabled`. |

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
  "no trust in network" answer — shipped in 0.19.0 as
  [`[sip.auth]`](#sipauth--inbound-digest-authentication-0190). Enable it
  (and set `auth_required` on the trunks that need it) for any
  internet-facing trunk without a static carrier IP.

## `[sip.auth]` — inbound digest authentication (0.19.0)

Challenge inbound INVITEs with RFC 3261 §22 / RFC 7616 digest auth, so
trust no longer rests on a spoofable network identity (source IP /
`From:` host). A new out-of-dialog INVITE that needs auth and arrives
without a valid `Authorization` is answered `401 Unauthorized` with a
`WWW-Authenticate` challenge (nonce, realm, qop); the peer re-sends the
INVITE with a digest `response` computed from its shared secret, which is
verified against the configured credentials. Replay is bounded by a
server nonce with a TTL; an expired nonce gets a `stale=true`
re-challenge (no user re-prompt). **Off by default.**

```toml
[sip.auth]
enabled   = true
realm     = "siphon.example"      # advertised in the challenge + folded into HA1
algorithm = "SHA-256"             # MD5 | SHA-256 (default) | SHA-512
qop       = "auth"                # auth (default) | auth-int

[[sip.auth.user]]
username = "carrier-a"
password = "${file:/run/secrets/carrier_a_sip}"   # keep it out of the file

[[sip.auth.user]]
username = "softphone-12"
password = "${cred:softphone_12_sip}"
```

| Field        | Type     | Default    | Notes |
|--------------|----------|------------|-------|
| `enabled`    | bool     | `false`    | Master switch. `false` ⇒ no INVITE is ever challenged. |
| `realm`      | string   | required if on | The digest realm advertised in the challenge and folded into HA1. Typically your SIP domain. Empty/missing while `enabled` → fatal at load. |
| `algorithm`  | enum     | `SHA-256`  | `MD5` \| `SHA-256` \| `SHA-512` (case-insensitive). MD5 is accepted for legacy peers but is weak (RFC 7616 §3) — prefer SHA-256. |
| `qop`        | enum     | `auth`     | `auth` \| `auth-int`. |
| `user[]`     | table array | ≥ 1 if on | At least one `[[sip.auth.user]]` when `enabled`. |
| `user[].username` | string | required | SIP username presented in `Authorization`. Duplicate usernames → fatal error. |
| `user[].password` | string | required | Shared secret. Held in memory as cleartext to recompute HA1 on verify (like `[[gateway]]`/`[[register]]`); use `${file:…}`/`${cred:…}` to keep it out of the config file. Empty → fatal error. |

### Which INVITEs get challenged

Digest is an **AND-gate with the `[[trunk]]` allowlist** — an INVITE must
pass the allowlist *and* digest. Which sources are challenged depends on
the trunk policy:

- **With `[[trunk]]` blocks:** only trunks that set `auth_required = true`
  are challenged. A static-IP carrier that doesn't send credentials stays
  allowlist-only (set `auth_required = false` / omit it) — so enabling
  `[sip.auth]` never breaks a trunk that isn't expecting a challenge.
- **No `[[trunk]]` blocks (legacy mode):** every new INVITE is challenged
  when `[sip.auth].enabled`.

In-dialog requests (re-INVITE, ACK, BYE) are never re-challenged. The
outcome is counted on `siphon_ai_sip_auth_total{result}`
(`ok`/`challenged`/`failed`/`stale`). `[sip.auth]` changes are
restart-required on `SIGHUP` (part of `[sip]`).

## `[sip.admission]` — inbound INVITE admission control (0.19.0)

A DoS posture beyond the `[[trunk]]` allowlist: shed abusive inbound
INVITEs **before** any trunk / auth / route work. Two independent,
optional limits — a **per-source token bucket** keyed on the source IP,
and a **global concurrency cap**. **Off by default** (omit the block, or
leave both `max_per_sec` and `max_concurrent` at `0`). Complements the
external `fail2ban` recipe (`docs/SECURITY_FAIL2BAN.md`) with an
in-process, immediate response.

```toml
[sip.admission]
max_per_sec    = 10     # per-source new-INVITE rate (token bucket); 0 = off
burst          = 20     # per-source bucket capacity; default = max_per_sec
drop_after     = 10     # consecutive per-source rejects → silent drop (not 503)
max_concurrent = 500    # global cap on concurrent active calls; 0 = off
max_sources    = 10000  # cap on tracked source IPs (bounded memory)
```

| Field        | Type | Default        | Notes |
|--------------|------|----------------|-------|
| `max_per_sec`    | int | `0` (off) | Per-source steady rate (tokens/sec), keyed on the transport source IP. A source over its rate is answered `503 Service Unavailable` + `Retry-After`. |
| `burst`          | int | `max_per_sec` | Per-source bucket capacity. Must be ≥ `max_per_sec` (a smaller burst is a fatal config error). |
| `drop_after`     | int | `10`      | After this many **consecutive** per-source rejects, further INVITEs from that source are **silently dropped** (no `503`) — an obvious flood doesn't earn a response. A single admitted INVITE resets the counter. |
| `max_concurrent` | int | `0` (off) | Global cap on concurrent active calls (read from the call registry). A new INVITE past the cap is answered `503`. Independent of `max_per_sec`. |
| `max_sources`    | int | `10000`   | Cap on distinct source IPs tracked. Idle (then oldest) entries are evicted past this, so the limiter can't leak memory under a spoofed-source flood. |

Admission runs as the **first** gate on a new out-of-dialog INVITE
(before drain / trunk / auth / route), so rejected load costs almost
nothing. Outcomes are counted on
`siphon_ai_invite_admission_total{result=accepted|rate_limited|dropped}`,
with `siphon_ai_invite_admission_sources` gauging the live table size.
`[sip.admission]` changes are restart-required on `SIGHUP` (part of
`[sip]`).

## `[outbound]` + `[[gateway]]` — outbound call origination (0.6.0)

SiphonAI can **place** calls (not just answer them) and bridge them to a WS
server on answer. Outbound is **disabled by default** and fail-closed: it
turns on only when `[outbound].max_concurrent` is a positive number. Full
guide (originate API, lifecycle, toll-fraud posture): `docs/OUTBOUND.md`.

```toml
[outbound]
max_concurrent     = 20      # 0 (default) = outbound disabled
rate_limit_per_sec = 5       # optional new-calls/sec ceiling (token bucket)

# A standalone trunk (static / IP-auth or digest):
[[gateway]]
name          = "twilio"
proxy         = "siptrunk.example.com:5060"
from          = "sip:+13125551234@siptrunk.example.com"   # caller-ID, sip: URI
auth_username = "ACxxxx"                                   # optional digest
auth_password = "${TWILIO_TRUNK_SECRET}"

# Or dial through an existing [[register]] (reuse its server + credentials):
[[gateway]]
name     = "pbx-out"
register = "pbx"            # name of a [[register]] block
# from = "sip:..."         # optional; defaults to the register AOR
```

| Field | Block | Notes |
|---|---|---|
| `max_concurrent` | `[outbound]` | Max simultaneous outbound calls. `0` (default) disables outbound entirely. This + `rate_limit_per_sec` are the **native guardrails** — the originate API itself has no built-in auth (it's fronted by a reverse proxy / trusted network), so **you must restrict access to that endpoint and set a sane cap** to avoid toll fraud. |
| `rate_limit_per_sec` | `[outbound]` | Optional ceiling on *new* outbound calls per second (token bucket, burst = the rate). `0`/unset = no rate limit. |
| `name` | `[[gateway]]` | Unique gateway name; the originate request names one. |
| `proxy` | `[[gateway]]` | `host` or `host:port` of the trunk (resolved per RFC 3263 at INVITE time). Required unless `register` is set. Default port: 5060, or 5061 when `transport = "tls"`. |
| `transport` | `[[gateway]]` | `"udp"` (default) \| `"tcp"` \| `"tls"`. Non-UDP dials out through the daemon's client connection pools; TLS verifies the trunk's cert against the webpki roots + `[sip.tls_client].extra_ca` and uses the proxy host as SNI. Must be unset when `register` is set — the transport is inherited from the register block. |
| `from` | `[[gateway]]` | Default caller-ID — a full `sip:`/`sips:` URI. Required for standalone trunks; defaults to the register AOR when `register` is set. |
| `register` | `[[gateway]]` | Name of a `[[register]]` to dial through, inheriting its server address, digest credentials, and AOR. |
| `auth_username` / `auth_password` | `[[gateway]]` | Digest credentials for a standalone trunk (both or neither). Answered on any 401/407 challenge the trunk sends. |
| `realm` | `[[gateway]]` | Optional digest realm hint. |
| `srtp` | `[[gateway]]` | SRTP policy for media on this trunk (0.7.x): `"off"` (default) \| `"preferred"` \| `"required"` — the outbound mirror of `[media].srtp`. `preferred` offers SDES SRTP (`RTP/SAVP` + `a=crypto:`) but accepts a plaintext downgrade; `required` fails the call if the trunk won't do SRTP. **Pair with `transport = "tls"`** — SDES carries the master key on the signalling plane, so a non-TLS trunk leaks it (warned at load). |
| `recording` | `[[gateway]]` | Default recording mode for calls placed through this gateway (0.26.0): `"off"` (default) \| `"always"` \| `"on_demand"` — same vocabulary as `[recording].mode`. A per-originate `recording` field overrides it. Requires `[recording].dir` when not `"off"` (validated at load). |

All of the above is validated at config load — unknown `register` references,
a `from` missing the `sip:` scheme, half-set credentials, duplicate names, or
a bad `proxy` all fail loud at startup.

## `[conference]` — conference rooms (0.7.0)

Multi-party rooms: N calls share one mixed audio room, every leg keeps its
own WS session, and each sink hears the room minus its own input (the caller
never hears themselves; each bot still hears its own caller, so STT keeps
working). Joins are driven over the WS protocol / admin API (0.7.0); this
block only declares the daemon-level facility.

```toml
[conference]
enabled = false                  # fail-closed, like [outbound]
max_rooms = 16
max_participants_per_room = 8
join_tones = false
```

| Field | Default | Notes |
|---|---|---|
| `enabled` | `false` | Off = every join refused. A 0.6.x config upgrades with zero behaviour change. |
| `max_rooms` | `16` | Live rooms across the daemon. Must be ≥ 1. |
| `max_participants_per_room` | `8` | Member **calls** per room (each contributes 2 mixer participants: its SIP leg and its WS session). Must be ≥ 2. Kept small on purpose — per-sink mixing cost grows quadratically with this cap. |
| `join_tones` | `false` | Short chime into the room on every join/leave. |

A room locks to its first joiner's negotiated sample rate (8 kHz or 16 kHz);
a join at a different rate is rejected — no resampling in 0.7.0 (documented
limitation). Rooms are created on first join and end when the last member
leaves. Global only — no per-route overrides (rooms are a daemon-level
facility, like outbound).

## `[park]` — media-only call park (0.7.0)

Park shelves a call **without** a WS session: the caller hears hold music,
the SIP dialog and RTP stay up, and the call is later **retrieved** onto a
*fresh* WS session (or times out / hangs up). Park is initiated by the WS
server (`park`, see `docs/PROTOCOL.md` §4.9) or an operator
(`POST /admin/v1/calls/:id/park`); retrieve is **operator-only**
(`POST /admin/v1/calls/:id/retrieve`). This block only declares the
daemon-level facility.

```toml
[park]
enabled = false                       # fail-closed, like [conference]
moh_file = "/etc/siphon-ai/moh.wav"   # optional; comfort noise if unset
timeout_secs = 300                    # 0 = park indefinitely
timeout_action = "hangup"             # "hangup" | "keep"
max_parked = 32
```

| Field | Default | Notes |
|---|---|---|
| `enabled` | `false` | Off = every park refused (`error { code: "park_failed" }`). A 0.6.x config upgrades with zero behaviour change. |
| `moh_file` | unset | Hold-music file looped while a call is parked. **Existence and decodability are checked at load** — a missing or garbage file fails startup loud. Unset → comfort noise. The file's native sample rate is resolved per-park; a call negotiated at a *different* rate falls back to comfort noise (no resampling in 0.7.0) — logged once, **not** a park failure. |
| `timeout_secs` | `300` | How long a call may stay parked before `timeout_action` fires. `0` disables the timeout (park indefinitely). |
| `timeout_action` | `"hangup"` | At timeout: `"hangup"` tears the call down; `"keep"` leaves it parked (the operator must retrieve or hang up). Any other value fails load. |
| `max_parked` | `32` | Max simultaneously-parked calls across the daemon. Must be ≥ 1. A park beyond the cap is refused (`park_failed`); the call continues unparked. |

Global only — no per-route overrides (park is a daemon-level facility, like
conference and outbound). Applies to inbound **and** outbound calls.

## `[security]` — STIR/SHAKEN call authentication

Verifies the RFC 8224 `Identity` header (RFC 8225 PASSporT, SHAKEN profile)
on inbound INVITEs — Identity parsing, ES256 signature, certificate-chain
validation to a STI-PA anchor, `orig`/`dest` claim binding, and `iat`
freshness — surfaces the verdict (`verstat`) on the WS `start`, the CDR, and
HEP, and optionally rejects calls below a configured attestation minimum.

With `[security.stir_shaken].enabled = false` (the default) the feature is
entirely inert — a 0.3.x config upgrades with no behaviour change.

**Read [`SECURITY_STIR_SHAKEN.md`](SECURITY_STIR_SHAKEN.md) before enabling
the gate** — it covers what attestation does and doesn't prove, the two
trust domains (`x5u` fetch TLS vs the SHAKEN chain), and observe-first
rollout.

```toml
[security]
min_attestation          = "none"   # "none" | "A" | "B" | "C"
min_attestation_response = 403       # 403 | 488 | 606

[security.stir_shaken]
enabled            = false           # master switch
trust_anchors      = "/etc/siphon-ai/sti-pa-roots.pem"
cert_cache_ttl_secs = 3600           # signing-cert cache TTL (seconds)
require_identity   = false           # reject unsigned INVITEs with 428
iat_freshness_secs = 60              # PASSporT iat replay window (0 disables)
```

### `[security]`

| Field | Type | Default | Notes |
|---|---|---|---|
| `min_attestation` | string | `"none"` | Minimum **trusted** attestation to admit a call: `A` (full) > `B` (partial) > `C` (gateway). `"none"` admits everything. A non-`none` value **requires** `stir_shaken.enabled = true` (else every call would be rejected — fail-loud config error). |
| `min_attestation_response` | int | `403` | SIP status when the gate rejects: `403` (Forbidden, recommended), `488`, or `606`. |

The gate admits a call only when verification **fully passed** and the claimed attestation meets the minimum. An unsigned call, a failed signature, or an attestation below the threshold is rejected — see the policy matrix:

| `min_attestation` | A | B | C | unsigned / header absent | invalid signature |
|---|---|---|---|---|---|
| `"none"` | ✓ | ✓ | ✓ | ✓ | ✓ |
| `"C"` | ✓ | ✓ | ✓ | reject | reject |
| `"B"` | ✓ | ✓ | reject | reject | reject |
| `"A"` | ✓ | reject | reject | reject | reject |

A rejected call gets the configured status plus a `Reason: Q.850;cause=21` header so a Homer/upstream sees *why* it was screened. The gate runs before media bring-up, so a rejected call never allocates an RTP port or WS bridge.

**Per-route override.** `[route.security].min_attestation` overrides the global for calls that match a route — a *strict* override (the route value fully replaces the global, even when more permissive), matching `[route.media].srtp` semantics. Unset → inherit the global. Like the global, a non-`none` override **requires** `stir_shaken.enabled = true` (fail-loud at load otherwise); `"none"` is an always-allowed no-op override.

```toml
[[route]]
name = "vip_inbound"
[route.match]
to_user = "2000"
[route.security]
min_attestation = "A"   # this route admits only fully-attested calls

[[route]]
name = "consumer_inbound"
[route.match]
any = true
[route.security]
min_attestation = "C"   # looser than a stricter global, by design
```

### `[security.stir_shaken]`

| Field | Type | Default | Notes |
|---|---|---|---|
| `enabled` | bool | `false` | Master switch. When off, no Identity parsing/verification runs and no `verstat` is surfaced. |
| `trust_anchors` | string | — | Path to the PEM bundle of STI-PA trust anchors. `contrib/sti-pa-roots.pem` is a **template** — populate it with the authentic STI-PA root(s) per `contrib/README.md` (we don't vendor a baked-in root; a stale/wrong anchor is a security defect). **Required when `enabled = true`**; validated at load time (must exist and contain ≥1 PEM certificate, so the unpopulated template fails loud by design). |
| `cert_cache_ttl_secs` | int | `3600` | How long a fetched signing certificate is cached before re-fetch. (Seconds, matching the other duration fields in this config.) |
| `require_identity` | bool | `false` | Reject inbound INVITEs that carry no `Identity` header with `428 Use Identity Header` (RFC 8224 §6.2.2) instead of admitting them as unsigned. |
| `iat_freshness_secs` | int | `60` | PASSporT `iat` freshness window, in seconds (replay protection, ATIS-1000074). The verdict's `iat_passed` is `false` when `iat` is more than this far from now (past **or** future), or absent. `0` disables the check (any `iat` passes) — an escape hatch for upstreams with broken clocks. |
| `x5u_tls_extra_ca` | string | — | Optional PEM bundle of extra CA cert(s) trusted **only** for the `x5u` HTTPS fetch — added to the public web-PKI roots, for operators hosting `x5u` behind a private/lab CA. Validated at load (exists + ≥1 cert) when `enabled`. **Note the two distinct trust domains:** this widens *fetch-TLS* trust; it does **not** affect the SHAKEN chain, which always validates against `trust_anchors`. Leave unset in production unless your `x5u` is privately hosted. |

## `[recording]` — call recording

Records each call's audio to a stereo WAV (caller = left channel, bot/WS =
right). Off by default. Recording runs off the audio hot path — a backed-up
writer can never stall live audio. **Full guide: `docs/RECORDING.md`.**

```toml
[recording]
mode   = "always"          # "off" (default) | "always" | "on_demand"
dir    = "/var/lib/siphon-ai/recordings"
format = "wav"             # "wav" (default) | "opus" (0.25.0, ~10× smaller)

[recording.encryption]                 # optional — encrypt at rest (0.24.0)
enabled = true                         # default false
kek     = "${file:/etc/siphon-ai/recording-kek.hex}"   # 64 hex chars
key_id  = "rec-2026-07"                # stamped into each recording
```

| Field | Type | Default | Notes |
|---|---|---|---|
| `mode` | string | `"off"` | `"off"` = no recording (zero behaviour change). `"always"` = record every accepted call for its full duration. `"on_demand"` = the WS server drives recording with `start_recording` / `stop_recording` / `pause_recording` / `resume_recording` (see `docs/PROTOCOL.md` §4.7); SiphonAI emits `recording_started` / `recording_stopped` / `recording_failed` back. (Per-route overrides land in a later 0.5.0 chunk.) |
| `format` | string | `"wav"` | `"opus"` writes Ogg-Opus instead of WAV — ~10× smaller for voice, encoded with the same libopus the media path uses. Extension becomes `.opus` (`.opusa` sealed). Playable by ffmpeg/VLC/browsers. |
| `dir` | string | — | Directory recordings are written to as `<dir>/<call_id>.<ext>` — `wav`/`wava` or `opus`/`opusa` by format × encryption. **Required when `mode != "off"`**; created at startup (a bad path fails loud at load). A `pause` omits the paused span from the file (the audio is dropped, not silenced). In-progress recordings are `<name>.part` and are renamed on finalize (0.24.0) — a bare `.wav`/`.wava` is always a complete file. |
| `encryption.enabled` | bool | `false` | Seal recordings into encrypted `.wava` envelopes (per-recording AES-256-GCM data key, wrapped by your `kek`). Decrypt offline with `siphon-ai decrypt-recording` — see `docs/RECORDING.md` §8 for the model, key rotation, and the container format. |
| `encryption.kek` | string | — | The key-encryption key as **64 hex characters** (32 bytes). Reference a secret — `${file:…}` or `${cred:…}` — never inline it. Required when `enabled`; validated at load (fail-loud). Generate one with `openssl rand -hex 32`. |
| `encryption.key_id` | string | — | Identifier (1–255 bytes) stamped into every recording's header, naming which KEK wrapped it — this is what makes rotation possible. Required when `enabled`. |
| `announcement.file` | string | — | A WAV played to the caller right after answer, **before any audio reaches the recording** — capture starts when the prompt finishes ("announce-then-bridge": the WS session connects in parallel; caller audio flows to the server after the prompt). Applies whenever the call records (`always` **and** `on_demand` — a server `start_recording` mid-prompt is deferred to prompt completion). The CDR gains `consent { announced, announcement_ms }`. Must exist at load; must be at the bridge rate (8/16 kHz) — a per-call rate mismatch **fail-closes** that call's recording (no capture without the prompt) and shows up as `consent.announced = false`. |
| `encryption.kms` | table | — | **AWS KMS as the KEK** (0.25.0) — instead of a local `kek` (exactly one of the two). `{ key_arn, region, access_key, secret_key, endpoint? }`; creds via `${cred:}`, `endpoint` only for KMS-compatible emulators. Each recording start is one KMS `Encrypt` (on the writer task, never the audio path; 10 s timeout → the *recording* fails, the call continues). Decrypt with `siphon-ai decrypt-recording --kms-region <r>` (+ `AWS_ACCESS_KEY_ID`/`AWS_SECRET_ACCESS_KEY`); the blob names its own key. |

**Object storage (`[recording.storage]`, 0.25.0).** Upload finalized
recordings to an S3-compatible bucket (AWS, MinIO, Cloudflare R2, Backblaze
B2 — path-style addressing, hand-rolled SigV4, no AWS SDK):

```toml
[recording.announcement]              # optional — consent prompt (0.26.0)
file = "/etc/siphon-ai/this-call-is-recorded.wav"

[recording.storage]
enabled      = true                      # default false
endpoint     = "https://s3.us-east-1.amazonaws.com"   # or a MinIO/R2 URL
bucket       = "call-recordings"
region       = "us-east-1"
access_key   = "${cred:s3-access-key}"
secret_key   = "${cred:s3-secret-key}"
key_template = "{date}/{call_id}"        # default; {route}/{direction} too
delete_local_after_upload = false        # default
spool_dir    = "/var/spool/siphon-ai/uploads"
```

| Field | Type | Default | Notes |
|---|---|---|---|
| `enabled` | bool | `false` | Spool each finalized recording for background upload. The CDR gains `recording_url` (`s3://bucket/key`) and a `recording_uploaded` lifecycle webhook fires per completed upload. |
| `endpoint` | string | — | Scheme + host (+ port). Required; must be `http(s)://`. |
| `bucket` / `region` | string | — | Required. For non-AWS targets `region` is whatever the endpoint expects (MinIO accepts any). |
| `access_key` / `secret_key` | string | — | Required. Reference secrets (`${cred:}` / `${file:}`), never inline. |
| `key_template` | string | `"{date}/{call_id}"` | Object-key template. Placeholders: `{call_id}`, `{date}` (UTC `YYYY-MM-DD`), `{route}`, `{direction}`. Must contain `{call_id}`; the file extension (`.wav`/`.wava`) is appended automatically. Validated at load. |
| `delete_local_after_upload` | bool | `false` | Delete the local file only after a **durable** upload. Retention/TTL beyond that belongs to the bucket's lifecycle policy — the daemon never schedules deletion. |
| `spool_dir` | string | — | Durable upload-job spool (survives restarts). Required; created at startup. |

Upload is best-effort and off every call path (CLAUDE.md §4.7): teardown
writes one small job file; a background worker uploads with retries (an
unreachable endpoint backs up in the spool, visible as
`siphon_ai_recording_upload_spool_depth`). Pair with
`[recording.encryption]` so the bucket only ever holds ciphertext.

**Per-route override.** A `[route.recording]` block overrides the global
`mode` for calls that match that route (strict override — the route value
fully replaces the global, like `[route.security]`). The output `dir` is
always the global one, so `[recording].dir` must be set whenever any route
enables recording — even if the global `mode = "off"`:

```toml
[recording]
mode = "off"                         # global default: don't record
dir  = "/var/lib/siphon-ai/recordings"

[[route]]
name = "support"
[route.match]
to = "5000"
[route.recording]
mode = "always"                      # …but always record the support line
```

Operational notes: recordings are uncompressed PCM16 stereo (≈115 MB/hour
at 16 kHz; ≈58 MB/hour at 8 kHz) — size your disk and **manage retention
yourself** (the daemon does not delete recordings). Plaintext WAV at rest
is fine for a lab; regulated deployments should turn on
`[recording.encryption]` (0.24.0). Recording consent and any "this call is
recorded" announcement are the operator's responsibility (see
`docs/SECURITY_STIR_SHAKEN.md` for the analogous trust-model framing).

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
secret     = "${CDR_WEBHOOK_SECRET}"          # optional HMAC signing (0.11.0)
spool_dir  = "/var/lib/siphon-ai/spool/cdr"   # optional durable retry (0.11.0)
retry_max  = 3
timeout_ms = 5000
```

Both sinks can run simultaneously. Master `enabled = false` silences both
regardless of sub-block state. Adding fields to the CDR schema bumps the
`version` field; consumers should treat new keys as additive.

The CDR webhook shares the delivery transport with `[webhooks]`, so
`secret` (HMAC `X-SiphonAI-Signature`), `spool_dir` (durable retry), the
`X-SiphonAI-Event-Id` idempotency header, and the `siphon_ai_webhook_*`
delivery metrics all behave identically here (0.11.0). The CDR JSON body is
unchanged — these are transport-layer headers/behavior, so the CDR schema
`version` is **not** bumped. See `docs/DEPLOY.md` → *Webhook delivery:
signing, idempotency, durability*.

## `[observability]`

| Field         | Type        | Default        | Notes |
|---------------|-------------|----------------|-------|
| `enabled`     | bool        | `false`        | Master switch for the HTTP server. |
| `http_listen` | `host:port` | required if on | Exposes `/metrics`, `/health`, `/ready`. **Since 0.10.0 it no longer serves `/admin/*`** (returns `404`) — admin moved to the authenticated `[admin]` listener below. |

This listener is unauthenticated by design (metrics/health are safe to
scrape). The privileged `/admin/*` surface lives on its own
[`[admin]`](#admin) listener.

### `[observability.otlp]` — OpenTelemetry trace export (0.22.0)

Export per-call **distributed traces** over OTLP/gRPC to a collector (Tempo /
Jaeger / an OpenTelemetry Collector). Each call is one trace — INVITE handling
→ controller → WS bridge → media — with the SIP `Call-ID`, direction, and
from/to on the root. **Off by default**, and **independent of the metrics
listener above**: you can export traces without `enabled = true` (and vice
versa), the same way HEP is independent of `[cdr]`.

Best-effort, like HEP (CLAUDE.md §4.7): spans batch on a background worker and
drop on overflow, so a slow or unreachable collector never blocks a call. A
bad *endpoint* fails loud at startup; a collector that's merely *down* does
not.

```toml
[observability.otlp]
enabled      = true
endpoint     = "http://localhost:4317"   # OTLP/gRPC collector
sample_ratio = 1.0                        # parent-based head sampling, [0.0, 1.0]
timeout_ms   = 5000                       # per-export gRPC timeout
service_name = "siphon-ai"                # service.name resource attribute

[observability.otlp.attributes]           # extra resource attributes
"deployment.environment" = "prod"
region                   = "us-east-1"
```

| Field          | Type       | Default                   | Notes |
|----------------|------------|---------------------------|-------|
| `enabled`      | bool       | `false`                   | Master switch. Off ⇒ the tracing layer is a zero-cost no-op. |
| `endpoint`     | URL        | `http://localhost:4317`   | OTLP/**gRPC** collector endpoint. |
| `sample_ratio` | float      | `1.0`                     | Head sampling ratio in `[0.0, 1.0]`; parent-based, so a sampled parent keeps its children. Out of range → fatal at load. |
| `timeout_ms`   | integer    | `5000`                    | Per-export gRPC timeout. |
| `service_name` | string     | `siphon-ai`               | `service.name` resource attribute. The node id is set as `service.instance.id`. |
| `attributes`   | table      | none                      | Extra resource attributes attached to every span. |

The daemon logs `OTLP trace export active` at startup when enabled, and
flushes pending spans on shutdown. See `docs/OPERATIONS.md` and
`examples/observability/` for the metrics/dashboards side.

Enabling OTLP also propagates the call's **W3C trace context to your WS
server** (0.23.0): the upgrade request carries `traceparent` (+ `tracestate`
when non-empty), and `start.trace_context` mirrors the same values (additive;
protocol stays v1 — see `docs/PROTOCOL.md` §3.1). A server that continues the
trace shows up in the same waterfall as the daemon's spans. There is no
separate knob: disabled OTLP ⇒ no headers, no field.

## `[admin]`

Authenticated admin API listener (0.10.0). Serves `/admin/*` (hangup,
**billable** origination, conference / park control, log-filter changes)
gated by a bearer token + role. **Omit `[admin]` entirely and `/admin/*`
is not served at all** — a secure default; the daemon still starts and
serves `[observability]`. Operator guide + the endpoint→role table:
`docs/DEPLOY.md` → *Admin API* / *Admin auth & RBAC*.

```toml
[admin]
listen = "127.0.0.1:9092"      # dedicated listener

[[admin.token]]                # one block per token; at least one required
name  = "ops-oncall"           # actor label in audit logs — NOT a secret
token = "${SIPHON_ADMIN_OP}"   # the bearer secret; ${VAR} expansion works
role  = "operator"             # readonly | operator | admin (roles nest)

[admin.tls]                    # optional — serve /admin/* over HTTPS (0.18.0)
cert = "${file:/etc/siphon/admin.crt}"   # PEM cert chain
key  = "${cred:admin_tls_key}"           # PEM private key
```

| Field            | Type        | Default        | Notes |
|------------------|-------------|----------------|-------|
| `listen`         | `host:port` | required if on | Where `/admin/*` is served. A non-loopback bind **without `[admin.tls]`** logs a warning (the bearer token would travel in the clear). |
| `token`          | table array | ≥ 1 required   | At least one `[[admin.token]]`; an `[admin]` block with no tokens is a fatal config error. |
| `token[].name`   | string      | required       | Unique, non-empty label recorded as the audit-log actor. Duplicate names are a fatal error. |
| `token[].token`  | string      | required       | The bearer secret (non-empty). Hashed (SHA-256) at load, compared in constant time, never logged. Use `${VAR}` / `${file:…}` / `${cred:…}` to keep it out of the file. |
| `token[].role`   | enum        | required       | `readonly` (GET/list) ⊂ `operator` (hangup, park/retrieve, conference CRUD) ⊂ `admin` (origination, `PUT /admin/log`, `hep/test`). Unknown value → fatal error. |
| `tls.cert`       | path        | required if `[admin.tls]` | PEM certificate chain. Present `[admin.tls]` with a missing/empty `cert` → fatal error. |
| `tls.key`        | path        | required if `[admin.tls]` | PEM private key. Loaded at startup (fail-loud) and **hot-reloaded on `SIGHUP`** alongside `[sip.tls]`, so cert rotation needs no restart. |

### `[admin.tls]` — encrypt the admin listener (0.18.0)

The bearer token authenticates the operator, but on a **plain-HTTP** listener
it travels in the clear — fine on `127.0.0.1`, not on a routable bind. Set
`[admin.tls]` to serve `/admin/*` over HTTPS directly (no proxy needed):

- Both `cert` and `key` are required when `[admin.tls]` is present; a missing
  or empty value fails the load.
- The cert is **hot-reloaded on `SIGHUP`** (same mechanism as `[sip.tls]`):
  the next connection picks up the new cert, in-flight ones keep theirs. A
  broken PEM on reload keeps the previous cert (nginx-style), never crashes.
  The `siphon_ai_admin_tls_reload_attempts_total{outcome}` counter tracks it.
- Without `[admin.tls]`, a non-loopback `listen` still works but logs a
  startup warning — bind loopback or front it with a TLS-terminating proxy.

Validation is at load time (CLAUDE.md §4.6): no tokens, an empty name or
secret, a duplicate name, an unknown role, an unparseable `listen`, or
`[admin.tls]` missing a cert/key all fail the daemon at startup rather than at
first request.

## `[webhooks]`

| Field         | Type     | Default                | Notes |
|---------------|----------|------------------------|-------|
| `enabled`     | bool     | `false`                | |
| `url`         | URL      | required if on         | POST target. |
| `auth_header` | string   | unset                  | Sent verbatim. `${VAR}` expansion works. |
| `secret`      | string   | unset                  | HMAC-SHA256 signing secret (0.11.0). When set, every POST carries `X-SiphonAI-Signature` for authenticity + replay protection. `${VAR}` expansion works; never logged. See `docs/DEPLOY.md` → *Webhook delivery: signing, idempotency, durability*. |
| `spool_dir`   | path     | unset                  | Durable retry spool directory (0.11.0). When set, a delivery that exhausts `retry_max` is persisted here and re-delivered by a background worker that survives restarts. Unset ⇒ best-effort (dropped after `retry_max`). Created + write-probed at startup (fail-loud). `${VAR}` expansion works. |
| `events`      | string[] | all                    | Allowlist. Valid today: `"call_start"`, `"call_end"`, `"registration_state_changed"`, `"outbound_initiated"`, `"outbound_answered"`, `"outbound_failed"`, `"conference_created"`, `"conference_ended"`, `"call_parked"`, `"call_retrieved"`, `"park_timeout"`, `"recording_uploaded"`. |
| `retry_max`   | integer  | `3`                    | In-memory retries before spooling (or dropping). |
| `timeout_ms`  | integer  | `5000`                 | |

Every delivery — webhook **and** CDR — also carries `X-SiphonAI-Event-Id`
(+ an `Idempotency-Key` alias): a stable id, reused across retries and any
spool replay, so a receiver can dedupe an at-least-once redelivery (0.11.0).

## `[audit]` — signed audit-event stream (0.20.0)

A tamper-evident trail of **admin and security decisions** for SIEM
ingestion: who touched the `[admin]` API, and what the daemon *refused*
on the SIP surface (failed auth, admission shedding, STIR/SHAKEN policy
rejection), plus config/cert reloads. Distinct from `[webhooks]` (ops
automation) and `[cdr]` (billing). **Off by default.**

Ships to an append-only JSONL **file** (for a log shipper) and/or an
HMAC-signed **webhook** (for a SIEM collector) — enable either or both.
The webhook reuses the same delivery transport as `[webhooks]`/`[cdr]`,
so `secret`, `spool_dir`, the `X-SiphonAI-Event-Id` idempotency header,
and the `siphon_ai_webhook_*` metrics (label `sink="audit"`) all behave
identically.

```toml
[audit]
enabled = true
# Optional allowlist; omit for all. Valid: "admin_request", "sip_auth",
# "invite_rejected", "attestation_rejected", "config_reload", "cert_reload".
events  = []

[audit.file]                   # append-only JSONL, one event per line
enabled = true
path    = "/var/log/siphon-ai/audit.jsonl"

[audit.webhook]                # HMAC-signed HTTP POST per event
enabled     = true
url         = "https://siem.example.com/ingest"
auth_header = "Bearer ${AUDIT_TOKEN}"
secret      = "${AUDIT_HMAC_SECRET}"           # STRONGLY recommended (tamper-evidence)
spool_dir   = "/var/lib/siphon-ai/spool/audit" # optional durable retry
retry_max   = 3
timeout_ms  = 5000
```

| Field                | Type     | Default        | Notes |
|----------------------|----------|----------------|-------|
| `enabled`            | bool     | `false`        | Master switch. `true` with **no** sub-sink enabled is a fatal config error (you'd think you're auditing but nothing records). |
| `events`             | string[] | all            | Event-type allowlist (see values above). Unknown names are accepted but never match. |
| `file.enabled`       | bool     | `false`        | |
| `file.path`          | path     | required if on | Append-only JSONL. Parent dir must exist (no `mkdir`); opened fail-loud at startup. |
| `webhook.enabled`    | bool     | `false`        | |
| `webhook.url`        | URL      | required if on | POST target. |
| `webhook.auth_header`| string   | unset          | Sent verbatim. `${VAR}` / `${file:}` / `${cred:}` expansion works. |
| `webhook.secret`     | string   | unset          | HMAC-SHA256 signing secret → `X-SiphonAI-Signature`. **Recommended** — the signature is what makes the stream tamper-evident. Unsigned logs a startup warning. |
| `webhook.spool_dir`  | path     | unset          | Durable retry spool (survives restarts). Unset ⇒ best-effort. Created + write-probed at startup. |
| `webhook.retry_max`  | integer  | `3`            | In-memory retries before spooling / dropping. |
| `webhook.timeout_ms` | integer  | `5000`         | Per-attempt timeout. |

**What is (and isn't) recorded, by design:** the stream captures the
*anomalies* a security team acts on, not routine call traffic. So
`invite_rejected` records admission `rate_limited` (503) and the `no_trunk`
/ `draining` refusals but **not** the per-packet silent flood-drop (that's
the DoS-shedding fast path — auditing it would amplify the attack).
`sip_auth` records `failed` / `stale` credentials but **not** the normal
first-leg `challenged` 401 or a successful `ok` (both track call volume,
not security). See `docs/AUDIT.md` for the full event schema.

## `[quality]` — per-call quality history records (0.31.0)

The **history half** of per-call quality telemetry: one JSON record per
call per `interval_secs`, plus a **final end-of-call summary**, in
exactly the shape of the CDR `quality` block (see DEPLOY.md) plus
framing (`kind`, `call_id`, `ts`, `seq`, `version`). Chart per-call
quality over time in *your* store — SiphonAI ships a clean, signed,
durable feed; it does not run a database. **Off by default.**

Ships to an append-only JSONL **file** (for a log shipper) and/or an
HMAC-signed **webhook** — enable either or both. The webhook reuses the
same delivery transport as `[webhooks]`/`[cdr]`/`[audit]`, so `secret`,
`spool_dir`, the `X-SiphonAI-Event-Id` idempotency header, and the
`siphon_ai_webhook_*` delivery metrics (label `sink="quality"`) all
behave identically. Restart-required — `[quality]` edits are flagged on
SIGHUP but not applied live.

```toml
[quality]
enabled       = true
interval_secs = 30              # per-call record cadence

[quality.file]                  # append-only JSONL, one record per line
enabled = true
path    = "/var/log/siphon-ai/quality.jsonl"

[quality.webhook]               # HMAC-signed HTTP POST per record
enabled     = true
url         = "https://metrics.example.com/ingest"
auth_header = "Bearer ${QUALITY_TOKEN}"
secret      = "${QUALITY_HMAC_SECRET}"
spool_dir   = "/var/lib/siphon-ai/spool/quality" # optional durable retry
retry_max   = 3
timeout_ms  = 5000
```

| Field                | Type    | Default        | Notes |
|----------------------|---------|----------------|-------|
| `enabled`            | bool    | `false`        | Master switch. `true` with **no** sub-sink enabled is a fatal config error. |
| `interval_secs`      | integer | `30`           | Per-call record cadence; must be ≥ 1. The final record is emitted regardless, whenever the call measured anything. |
| `file.enabled`       | bool    | `false`        | |
| `file.path`          | path    | required if on | Append-only JSONL. Parent dir must exist (no `mkdir`); opened fail-loud at startup. |
| `webhook.enabled`    | bool    | `false`        | |
| `webhook.url`        | URL     | required if on | POST target. |
| `webhook.auth_header`| string  | unset          | Sent verbatim. `${VAR}` / `${file:}` / `${cred:}` expansion works. |
| `webhook.secret`     | string  | unset          | HMAC-SHA256 signing secret → `X-SiphonAI-Signature`. Unset ⇒ unsigned deliveries. |
| `webhook.spool_dir`  | path    | unset          | Durable retry spool (survives restarts). Unset ⇒ best-effort. |
| `webhook.retry_max`  | integer | `3`            | In-memory retries before spooling / dropping. |
| `webhook.timeout_ms` | integer | `5000`         | Per-attempt timeout. |

Records with nothing measured are skipped (early ticks before the first
media-stats snapshot, calls that never went active), so consumers never
see empty rows. Counters inside a record are **cumulative since call
start** — diff successive `seq`s of the same `call_id` for rates. See
`docs/OPERATIONS.md` for the end-to-end ingestion pipeline
(webhook/file → Vector → Loki → Grafana).

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

## `[shutdown]`

Graceful connection draining on a shutdown signal (0.17.0). On `SIGTERM`
or `SIGINT`, instead of tearing active calls down immediately, the daemon
enters a *draining* state: it flips `/ready` to not-ready (so a load
balancer stops routing to it), answers **new** inbound INVITEs with `503
Service Unavailable` + `Retry-After` (so an upstream proxy routes
elsewhere), and lets in-flight calls finish — bounded by a timeout — before
exiting. In-dialog requests (re-INVITE for hold/resume, ACK, BYE) for calls
already up keep flowing so they can drain cleanly. See
`docs/design/DESIGN_GRACEFUL_SHUTDOWN.md`.

```toml
[shutdown]
drain_timeout_secs = 30   # default
```

| field                | type | default | meaning |
|----------------------|------|---------|---------|
| `drain_timeout_secs` | int  | `30`    | Max seconds to let active calls finish before forcing teardown. `0` = **no drain** (immediate exit, pre-0.17.0 behaviour). |

Omitting `[shutdown]` entirely gives the 30 s default. The value **must be
≤ your orchestrator's `terminationGracePeriodSeconds`** (k8s) or
`TimeoutStopSec` (systemd), or the supervisor `SIGKILL`s the daemon
mid-drain. Calls still active when the timeout fires are **force-terminated
gracefully** — a real SIP `BYE` to the peer and a WS `hangup` — and counted
on `siphon_ai_calls_drain_forced_total` (and attributed `drain_forced` on the
CDR / `siphon_ai_calls_total`). A **second** shutdown signal during the drain
(operator Ctrl-C twice, or a re-sent SIGTERM) forces immediate teardown,
dropping any remaining calls without a BYE. Observe the drain with the
`siphon_ai_draining` gauge (1 while draining) and the `siphon_ai_drain_seconds`
histogram (how long the drain took).

## Validating, inspecting & reloading config

The daemon has read-only subcommands for working with a config file without
starting it, plus `SIGHUP` hot-reload for a subset of sections (0.12.0).
Running the daemon is unchanged — `siphon-ai --config X` with no subcommand.

### `siphon-ai check --config X`

Validate and compile the config, then exit — **no sockets, no runtime**.
Exit `0` (with a one-screen summary) if valid, `1` (with the error on
stderr) otherwise. The CI / pre-deploy / pre-`systemctl reload` preflight.

```sh
siphon-ai check --config /etc/siphon-ai/config.toml || echo "bad config, not deploying"
```

A missing default route (`any = true`) warns but still exits `0` (matches
the daemon's startup behavior).

### `siphon-ai print-config --config X [--show-secrets]`

Print the **effective** compiled config (post-`${VAR}`, post per-route
merge) so you can see what your file actually resolved to — which `${VAR}`
won, what each route inherits vs overrides. **Secrets are redacted** (auth
headers, signing secrets, register/gateway passwords, admin token hashes,
HEP password → `<redacted>`); `--show-secrets` reveals them for local
debugging.

### `siphon-ai route-test --config X --to N [...]`

Run the dialplan against a synthetic call (first-match-wins) and report the
winning route — or `NO MATCH → SIP 404` — plus its effective `ws_url` /
codecs (route override vs `[bridge]` default). Flags: `--to` / `--from` /
`--ruri-user` / `--ruri-host` / `--to-host` / `--from-host` /
`--register-source` (default `trunk`) / `-H 'Name: Value'` (repeatable).
`--ruri-user` defaults to `--to`.

```sh
siphon-ai route-test --config x.toml --to 1000 --from sip:alice@pbx --register-source trunk
```

### Hot reload (`SIGHUP` / `systemctl reload`)

`SIGHUP` re-reads the **same `--config` file** and hot-applies the
reload-safe sections without dropping calls:

- **routes** — new INVITEs use the new dialplan; in-flight calls keep the
  route they matched;
- **webhook + CDR sinks** (`[webhooks]`, `[cdr]`) — rebuilt and swapped,
  **unless** a durable spool (`spool_dir`) is active for that sink (its
  background drain worker can't be hot-swapped → restart required);
- the **audit sink** (`[audit]`, 0.20.0) — rebuilt and swapped the same
  way, but **only when `[audit]` was enabled at startup**; turning it on
  from off is restart-required (the process-global facade is installed
  once at boot). Disabling or retargeting an already-on stream is hot;
- **outbound gateways** (`[[gateway]]`, 0.12.1) — the set is rebuilt and
  swapped (add / remove / modify trunks, **including rotating a gateway's
  `auth_password`**); in-flight outbound calls keep the trunk they're on.
  Requires outbound enabled and the `[outbound]` limits unchanged (see below);
- the **`[sip.tls]` cert** is reloaded too (the 0.3.0 behavior, unchanged).

**Fail-safe:** if the new config doesn't load/compile, the error is logged,
the running config is **kept**, and `siphon_ai_config_reloads_total{result="failed"}`
ticks — a bad edit can't take the daemon down. Run `siphon-ai check` first.

**Restart-required sections.** Everything else binds a socket, builds
process-wide state, or spawns tasks at startup, and a reload that changes any
of it applies the safe sections above and **logs a warning naming the
section(s)** that did not take effect (it is never silently swallowed) —
`[node]`, `[sip]`, `[media]`, the `[bridge]`/codec defaults (`[media].codecs`
/ `.dtmf` compile in here), `[[trunk]]`, `[[register]]`, `[security]` (incl.
`min_attestation`), `[recording]`, `[conference]`, `[park]`,
`[observability]`, `[admin]` (incl. the token table — a rotated/revoked admin
token keeps working **until restart**), `[hep]`, `[shutdown]` (the drain
window is read once at startup), and the `[outbound]` limits
(`max_concurrent` / `rate_limit_per_sec`, which also flip outbound on/off —
resizing the live admission semaphore isn't safe).

Watch `siphon_ai_config_reloads_total{result}` (`applied` / `no_change` /
`failed`); each reload also logs what it did. See `docs/DEPLOY.md` for the
`systemctl reload` flow.
