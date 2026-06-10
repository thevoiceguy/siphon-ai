# Deployment Guide

This is the operator's reference for running `siphon-ai` in something other
than `cargo run`. For configuration semantics see `docs/CONFIG.md`; for the
observability bar (the §11.8 ten questions) see `docs/OPERATIONS.md`.

## Container image

The repo ships a multi-stage Dockerfile that builds a statically-linked
musl binary on Alpine and copies it into a fresh runtime image:

```sh
docker build -f docker/Dockerfile -t siphon-ai:dev .
```

Target size: ~31 MB. No glibc/musl ABI gotchas at deploy time. If you want
to shave another ~7 MB, swap the runtime base for `scratch` or
`distroless/static`.

A turnkey `docker compose up` stack lives in `docker/compose.yaml`; it
brings up the daemon plus the reference Python echo WS server. See the
README quickstart for the demo flow.

## Required ports

The daemon binds the following by default. Adjust to taste, but make sure
everything in this table is reachable end-to-end between the SIP peer, the
operator's network, and the daemon container.

| Port              | Proto  | Source         | Direction | What flows here |
|-------------------|--------|----------------|-----------|-----------------|
| `[sip].listen`    | UDP    | TOML           | inbound   | SIP signaling (default 5060 / 5070 in samples). Bidirectional within UDP. |
| `[sip].listen`    | TCP    | TOML           | inbound   | Same port number when `transports` includes `"tcp"`. |
| `[sip.tls].listen`| TCP    | TOML           | inbound   | TLS signaling. Defaults to the SIP IP + 5061. |
| `[media].rtp_port_range` | UDP | TOML       | both      | RTP/RTCP. Forge allocates one even-numbered RTP port + the next odd RTCP port per call. Forward the whole range. |
| `[observability].http_listen` | TCP | TOML  | inbound (cluster-local) | `/metrics`, `/health`, `/ready`, `/admin/*`. Don't expose this to the public internet — `/admin/*` has no auth in v1. |
| Outbound, dynamic | TCP    | `[bridge].ws_url` (per route) | outbound | WebSocket from daemon to operator's WS server. |
| Outbound, dynamic | TCP    | `[cdr.webhook].url`, `[webhooks].url` | outbound | HTTP POSTs for CDRs and lifecycle webhooks. |
| Outbound 9060     | UDP    | `[hep].collector` | outbound | HEP3 to Homer. UDP only in v1. |
| Outbound 5060/5061 | UDP/TCP | `[[register]].server` | bidirectional | Per `[[register]]` block. |

## TLS deployment (SIP/TLS + WSS)

A production deployment encrypts both legs: SIP/TLS for signaling
to the carrier or PBX, and WSS for the bridge to the WS server.
The mechanics already ship in v0.1.0; this is the recipe.

### 1. Obtain a certificate

`siphon-ai` reads a PEM cert chain + PEM private key from disk —
any provisioning path works. Common options:

| Source | When to use | Notes |
|--------|-------------|-------|
| **Let's Encrypt (DNS-01)** | Public SIP-on-Internet, the carrier accepts a public CA. | Use DNS-01 so the daemon doesn't need port 80; renewals are unattended via certbot's deploy-hook. |
| **Carrier-issued / pinned** | The carrier signs your cert or expects a specific intermediate. | Drop the carrier's chain in as `cert`. The private-CA bundle goes in your OS trust store if you also need to *verify* the carrier's leaf. |
| **Internal PKI** | Site-to-site to your own PBX (e.g. Asterisk, CUCM). | Both sides trust an internal root. Put the root in `/etc/ssl/certs/` so rustls picks it up via the system store path you've configured. |

The cert's CommonName / SubjectAltName must include the hostname
the carrier or PBX resolves for your trunk — usually the same name
you put in `[node].public_address`.

### 2. Configure `[sip.tls]`

```toml
[sip]
listen     = "0.0.0.0:5060"
transports = ["udp", "tcp", "tls"]   # `"tls"` requires the block below

[sip.tls]
listen = "0.0.0.0:5061"              # standard SIP/TLS port
cert   = "/etc/siphon-ai/tls/fullchain.pem"
key    = "/etc/siphon-ai/tls/privkey.pem"
```

Both `cert` and `key` are paths on disk; the daemon loads them at
startup via `sip_transport::load_rustls_server_config` and binds
the listener before answering `/ready`. A missing or unreadable
file fails fast at startup with a clear error — no silent fallback
to UDP.

> **Inbound UAS only in v0.1.0/0.2.0.** Outbound TLS connections
> (UAC originating a new TLS dialog) are not implemented and
> return a clear error rather than silently downgrading. Inbound
> `INVITE sips:…` from the carrier works.

### 3. WSS to the WebSocket server

Just set `wss://` in `[bridge].ws_url` (or `[route.bridge].ws_url`):

```toml
[bridge]
ws_url = "wss://reception.example.com/sip-bridge"
```

No client cert or extra config is needed. The daemon's
`tokio-tungstenite` client is built with `rustls-tls-webpki-roots`
— the Mozilla CA bundle is baked into the binary, so trust works
out-of-the-box for any publicly-signed cert without depending on
the host's CA store. For an internal CA, the simpler path is to
terminate WSS at a reverse proxy with a publicly-trusted cert in
front of your WS server.

`[bridge].ws_auth_header` works identically over WSS — use it for
the bearer token your WS server expects:

```toml
ws_auth_header = "Bearer ${BRIDGE_TOKEN}"
```

#### 3a. mTLS to the WebSocket server (0.3.0+)

When the WS server requires a client certificate (carrier-pinned
deployments, internal-only services with a private CA), configure
the client cert + key via `[bridge.tls]`:

```toml
[bridge]
ws_url = "wss://reception.example.com/sip-bridge"

[bridge.tls]
client_cert = "/etc/siphon-ai/bridge/client.pem"  # PEM chain, leaf first
client_key  = "/etc/siphon-ai/bridge/client.key"  # PEM private key
# Optional: pin a single server cert by SHA-256 of its
# SubjectPublicKeyInfo. When set, replaces the default Mozilla CA
# verification — the connection only succeeds against this exact
# cert. Survives cert rotation as long as the operator keeps the
# same key pair (RFC 7469 §3).
# pinned_sha256 = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
```

Field semantics:

- `client_cert` — PEM-encoded chain. Must contain at least the leaf
  cert that authenticates this siphon-ai instance to the WS server.
  Intermediates allowed.
- `client_key` — PEM-encoded private key matching `client_cert`'s
  leaf. PKCS#8 / RSA / SEC1 all supported (whatever `rustls-pemfile`
  recognises).
- `pinned_sha256` — optional 64-hex-char SHA-256 of the server's
  SubjectPublicKeyInfo DER. To compute from a server cert:
  ```
  openssl x509 -in server.pem -pubkey -noout | \
    openssl pkey -pubin -outform der | sha256sum
  ```
  Lowercase or uppercase hex, no `:` separators, no `sha256/` prefix.

Validation happens at daemon startup — bad PEM, mismatched key, or
malformed pin all fail the config-compile step with a clear error
before any inbound INVITE is accepted.

`[bridge.tls]` is global only in 0.3.0; per-route override
(`[route.bridge.tls]`) is a 0.3.1 follow-up.

### 4. File permissions for cert/key

The systemd unit runs as the unprivileged `siphon` user; that user
must be able to *read* the cert and key but should not own them.

```sh
sudo install -d -m 0750 -o root -g siphon /etc/siphon-ai/tls
sudo install -m 0640 -o root -g siphon fullchain.pem /etc/siphon-ai/tls/
sudo install -m 0640 -o root -g siphon privkey.pem   /etc/siphon-ai/tls/
```

`ProtectSystem=strict` in the unit blocks writes outside
`/etc/siphon-ai/`, which is fine because renewal tools write to
the cert directory directly.

### 5. Renewal

`siphon-ai` 0.3.0+ supports **hot cert reload** via `SIGHUP`: the
daemon re-reads `[sip.tls].cert` + `.key` from disk and rotates
the listener's `ServerConfig` without dropping in-flight TLS
sessions (RFC 5746-compliant rotation — existing dialogs keep
using the cert they handshook with; new dialogs pick up the
fresh cert). The systemd unit's `ExecReload=` wires `systemctl
reload siphon-ai` to the SIGHUP.

```sh
# Let's Encrypt deploy-hook (/etc/letsencrypt/renewal-hooks/deploy/)
#!/bin/sh
set -e
install -m 0640 -o root -g siphon \
    "$RENEWED_LINEAGE/fullchain.pem" /etc/siphon-ai/tls/
install -m 0640 -o root -g siphon \
    "$RENEWED_LINEAGE/privkey.pem"   /etc/siphon-ai/tls/
systemctl reload siphon-ai
```

#### What survives, what doesn't

| | In-flight TLS dialogs | New TLS connections |
|---|---|---|
| Before reload | Use cert at process start | (n/a) |
| **During reload** | Keep using cert at process start — *no renegotiation, no drop* | Picked from the new cert on accept |
| After reload   | Same as before — handshook with the old cert, life-of-the-call | Use the new cert |

The `siphon_ai` unit increments
`siphon_ai_sip_tls_reload_attempts_total` on each SIGHUP (with
`outcome="ok"` / `"failed"` label) so you can alert on a stuck
renewal.

#### Failure handling

A broken PEM file on reload does **not** kill the daemon: the
new-config load fails, an `error!` is logged with the parser
diagnostic, and the previous `ServerConfig` keeps serving. Same
shape as `nginx -s reload`: if the new config is bad, the
running config keeps going.

#### Restart-on-renewal fallback

If you need to roll the cert older-school (e.g., a deployment
pipeline that always restarts services on config change), the
0.2.0 recipe still works — replace `systemctl reload` with
`systemctl restart`. A restart drops in-flight calls; SIGHUP
doesn't.

### 6. Smoke test

```sh
# From outside the daemon: confirm the TLS listener answers and
# presents the expected cert.
openssl s_client -connect siphon.example.com:5061 -servername siphon.example.com \
    -showcerts < /dev/null 2>&1 | head -20

# Verify your trunk peer can route a SIPS INVITE end-to-end.
# SIPp's `-t l1` enables TLS:
sipp -sn uac -t l1 -tls_cert client.pem -tls_key client.pem \
     siphon.example.com:5061 -m 1 -s 1000
```

If the listener answers but the carrier sees handshake failures,
the usual cause is a missing intermediate in `fullchain.pem` —
verify with `openssl s_client -showcerts` that the full chain is
present, not just the leaf.

## systemd unit (sketch)

A minimal unit file. Put the config under `/etc/siphon-ai/`, the binary
under `/usr/local/bin/`, run as a non-root user, give it cap_net_bind only
if you must listen below 1024.

```ini
[Unit]
Description=SiphonAI — SIP-to-WebSocket bridge
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=siphon
Group=siphon
EnvironmentFile=-/etc/siphon-ai/env
ExecStart=/usr/local/bin/siphon-ai --config /etc/siphon-ai/siphon-ai.toml
# SIGHUP triggers SIP/TLS cert hot-reload (0.3.0+). `systemctl
# reload siphon-ai` invokes this — see §5 above for renewal flow.
ExecReload=/bin/kill -HUP $MAINPID
Restart=always
RestartSec=5
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
PrivateTmp=true
LimitNOFILE=65536

[Install]
WantedBy=multi-user.target
```

`/etc/siphon-ai/env` is the right place for `BRIDGE_TOKEN=…`, `HEP_PASSWORD=…`,
and any other secrets your TOML references via `${VAR}`. `systemctl
edit siphon-ai` is fine for per-host overrides.

## Prometheus scrape

```yaml
scrape_configs:
  - job_name: siphon-ai
    scrape_interval: 15s
    static_configs:
      - targets: ['siphon-ai.internal:9091']
```

The metrics surface is documented under §Metrics below. All metrics carry
the `siphon_ai_` prefix unless they come from forge-media (`forge_*`) or
the heplify collector (`heplify_*`).

## Health checks

| Endpoint  | Method | When it returns 200                                          |
|-----------|--------|--------------------------------------------------------------|
| `/health` | GET    | The daemon process is up. Use as a liveness probe.           |
| `/ready`  | GET    | Daemon is fully bootstrapped — SIP transports bound, every `[[register]]` row has had a chance to settle. Use as a readiness probe. |

Both live on the `[observability]` listener.

## Admin API

`/admin/*` lives on the same port as `/metrics`. **No auth in v1** — keep
the listener on a private network.

| Method | Path                          | Body            | Purpose |
|--------|-------------------------------|-----------------|---------|
| GET    | `/admin/calls`                | —               | List active per-call SIP Call-IDs. |
| POST   | `/admin/calls/:id/hangup`     | —               | Force-shutdown a specific call by Call-ID. |
| GET    | `/admin/registrations`        | —               | Snapshot of every `[[register]]` row and its current state. |
| GET    | `/admin/log`                  | —               | Current `tracing` filter directive. |
| PUT    | `/admin/log`                  | text directive  | Replace the filter (e.g., `siphon_ai=info,siphon_ai_bridge=debug`). Returns the previous filter. |
| POST   | `/admin/v1/calls`             | JSON (below)    | **Originate an outbound call** (0.6.0). Returns `202 {"call_id": "..."}`; the call proceeds asynchronously. `501` when `[outbound]` is disabled. |

### `POST /admin/v1/calls` — outbound origination

Requires `[outbound].max_concurrent > 0` and at least one `[[gateway]]` (see
`docs/CONFIG.md`; full guide: `docs/OUTBOUND.md`). **This endpoint places billable calls and has no built-in
auth** — restrict access to it (bind to a private network / front with an
authenticating reverse proxy). The `max_concurrent` cap + `rate_limit_per_sec`
are the native guardrails.

```sh
curl -X POST http://localhost:9091/admin/v1/calls -d '{
  "to": "+15558675309",
  "gateway": "twilio",
  "ws_url": "wss://my-bot.example/outbound"
}'
# → 202 {"call_id":"siphon-…"}
```

| Field | Required | Notes |
|---|---|---|
| `to` | yes | Dialed destination (E.164 / SIP user) — the Request-URI user dialed through the gateway. |
| `gateway` | yes | Name of a `[[gateway]]`. `404` if unknown. |
| `ws_url` | no | WS server to bridge the answered call to. Falls back to `[bridge].ws_url`; `400` if neither is set. |
| `from` | no | Caller-ID override (`sip:` URI). Falls back to the gateway's `from`. |

Responses: `202` (accepted — placing), `404` (unknown gateway), `400` (bad
target / no ws_url / invalid JSON), `503` (`max_concurrent` reached), `429`
(rate limited), `501` (outbound disabled). The call's progress arrives
out-of-band via lifecycle webhooks: `outbound_initiated`, then exactly one
of `outbound_answered` (followed by `call_end` + a CDR when the call
finishes) or `outbound_failed` (see [Lifecycle webhooks](#lifecycle-webhooks)).

Example: bump bridge logging to debug for an incident, then revert.

```sh
prev=$(curl -s http://localhost:9091/admin/log)
curl -X PUT --data 'siphon_ai=info,siphon_ai_bridge=debug' \
    http://localhost:9091/admin/log
# … reproduce the issue …
curl -X PUT --data "$prev" http://localhost:9091/admin/log
```

## CDR consumers

When `[cdr.file]` is enabled, the daemon appends one JSON record per ended
call to the configured path. Rotate the file with `logrotate`; the daemon
re-opens on `SIGHUP` (in practice — restart is simpler).

```json
{
  "version": 1,
  "call_id": "siphon-6ce27797cc0a4997b90cbae2f46ce7a4",
  "sip_call_id": "1-2651348@127.0.0.1",
  "started_at": "2026-05-12T18:10:32.481Z",
  "ended_at":   "2026-05-12T18:11:04.117Z",
  "duration_ms": 31636,
  "from": "sipp",
  "to":   "1000",
  "direction": "inbound",
  "route": "default",
  "ws_url": "ws://echo-ws:8765/",
  "audio":   { "codec": "PCMU", "payload_type": 0, "sample_rate": 8000 },
  "termination": {
    "cause": "local_shutdown",
    "bridge_disconnect": "stop_sent",
    "tap_disconnect":    "call_ended"
  }
}
```

`termination.cause` values: `"server_hangup"`, `"local_shutdown"`,
`"bridge_ended"`, `"tap_ended"`. `tap_disconnect` adds
`"inactivity_timeout"` when the RTP watchdog fired. New fields are
additive; the `version` integer bumps only on breaking changes.

Two optional STIR/SHAKEN fields appear when `[security.stir_shaken]` is
enabled (added in 0.4.0; schema stays at version 1 — both are omitted
entirely when verification is disabled):

- `verstat_attest` — claimed attestation, `"A"` / `"B"` / `"C"`. Present
  only when the `Identity` header carried a valid attestation claim;
  omitted for unsigned calls.
- `verstat_passed` — composite verification result (`true` only when the
  signature, certificate chain, and orig/dest checks all passed). Emitted
  for every inbound call while verification is on, including `false` for
  unsigned or failed calls.

`verstat_attest` is the *claimed* level; a CDR with `verstat_attest: "A"`
and `verstat_passed: false` is a call that asserted full attestation but
failed verification.

Two optional recording fields appear when the call was recorded (added in
0.5.0; schema stays at version 1 — both omitted when recording is off):

- `recording_id` — identifies the recording (equals `call_id` in this
  release).
- `recording_path` — filesystem path of the WAV. Present even when the
  recording `failed` (it points at where the file would be); cross-check
  with the `siphon_ai_recordings_total` metric for the outcome.

Outbound originated calls (0.6.0, `POST /admin/v1/calls`) produce the same
record with `direction: "outbound"` — the schema stays at version 1 (the
field was reserved for this since v1). Two outbound-specific readings:

- `route` carries the `[[gateway]]` name the call was placed through, not
  a `[[route]]` name.
- `started_at` is when the INVITE went out, so `duration_ms` includes ring
  time; the answer instant is on the `outbound_answered` webhook.

Only *answered* outbound calls get a CDR — calls that end busy / declined /
unanswered / unreachable are covered by the `outbound_failed` webhook and
the `siphon_ai_outbound_calls_total{result}` metric, mirroring inbound
where CDRs cover bridged calls only.

The webhook sink delivers the same JSON to `[cdr.webhook].url` with
`Content-Type: application/json`. Retries on non-2xx up to
`[cdr.webhook].retry_max` times with exponential backoff.

## Lifecycle webhooks

Off-band events (NOT the per-call WS bridge). Same retry semantics as the
CDR webhook. Event types:

| `type`                          | When                                             |
|---------------------------------|--------------------------------------------------|
| `call_start`                    | After 200 OK has gone out on an accepted INVITE. |
| `call_end`                      | After the controller exits and the CDR record is built (inbound *and* answered outbound calls). |
| `registration_state_changed`    | Each `[[register]]` state transition (`pending` → `registered`, `registered` → `failed`, etc.). |
| `outbound_initiated`            | An originated call (`POST /admin/v1/calls`) was admitted and its INVITE is going out. |
| `outbound_answered`             | The callee answered (2xx) and the WS bridge is starting. |
| `outbound_failed`               | The originated call ended without an answer. Terminal — no `call_end`/CDR follows. |

Each delivery is a single JSON object with `version`, `timestamp` (ISO 8601), `type`,
and per-event fields documented in `crates/webhooks/src/event.rs`.

An outbound call emits `outbound_initiated`, then exactly one of
`outbound_answered` or `outbound_failed`, all sharing the `call_id` that
`POST /admin/v1/calls` returned. Answered calls finish with a `call_end`
(same shape as inbound; `route` = gateway name). `outbound_failed.cause`
mirrors the `siphon_ai_outbound_calls_total{result}` metric labels:
`busy` / `declined` / `no_answer` / `rejected` / `unreachable` / `failed`.

```json
{ "type": "outbound_initiated", "version": 1, "call_id": "siphon-…",
  "timestamp": "2026-06-09T10:00:00Z", "to": "+15558675309", "gateway": "twilio" }
{ "type": "outbound_answered", "version": 1, "call_id": "siphon-…",
  "sip_call_id": "f81d4fae…@10.0.0.5", "timestamp": "2026-06-09T10:00:06Z" }
{ "type": "outbound_failed", "version": 1, "call_id": "siphon-…",
  "timestamp": "2026-06-09T10:00:30Z", "cause": "no_answer" }
```

## HEP / Homer

See `docs/HEP.md` for the architecture and `examples/homer-stack/` for a
local Homer + heplify-server + Postgres compose stack.

Quick check that emission is live:

```sh
curl -s http://localhost:9091/metrics | grep siphon_ai_hep
```

`siphon_ai_hep_collector_up` should be `1`; `siphon_ai_hep_packets_sent_total`
should be incrementing across calls.

## Metrics

All histograms have sensible default buckets defined explicitly — no reliance
on the metrics crate's defaults (CLAUDE.md §7.4).

| Metric                                  | Type      | Labels                                | What it measures |
|-----------------------------------------|-----------|---------------------------------------|------------------|
| `siphon_ai_invites_total`               | counter   | `result=accepted\|rejected\|rejected_attestation\|no_match` | INVITEs by acceptance outcome. `rejected_attestation` is a STIR/SHAKEN policy reject (`min_attestation` gate or `require_identity`) — separately alertable from ordinary routing/media `rejected`. |
| `siphon_ai_calls_total`                 | counter   | `cause=server_hangup\|local_shutdown\|bridge_ended\|tap_ended` | Ended calls by termination cause. |
| `siphon_ai_calls_active`                | gauge     | —                                     | Currently-running calls. |
| `siphon_ai_route_match_total`           | counter   | `route`                               | Calls per matched route. |
| `siphon_ai_verstat_total`               | counter   | `result=passed\|failed\|unsigned`     | STIR/SHAKEN verification outcomes per inbound INVITE. Emitted only when `[security.stir_shaken].enabled = true`. `passed` = every check held; `failed` = `Identity` header present but verification didn't fully pass; `unsigned` = no `Identity` header. |
| `siphon_ai_recordings_total`            | counter   | `result=ok\|degraded\|failed`         | Recordings finished, when `[recording]` is on. `ok` = written cleanly; `degraded` = some 20 ms frames dropped under writer back-pressure (file is short, not corrupt); `failed` = an I/O error. |
| `siphon_ai_outbound_calls_total`        | counter   | `result=answered\|busy\|declined\|no_answer\|rejected\|unreachable\|failed` | Outbound calls placed (0.6.0). `answered` = 2xx + bridged; `busy` = 486/600; `declined` = 403/603; `no_answer` = 408/480/487; `rejected` = other non-2xx; `unreachable` = DNS/transport/timeout with no response; `failed` = local media setup error. |
| `siphon_ai_transfers_total`             | counter   | `mode=blind\|attended`, `result=accepted\|rejected\|local_error` | REFER transfers attempted (0.6.1; also counts blind transfers, previously unmetered). `accepted` = 202 + call torn down; `rejected` = peer non-2xx; `local_error` = bad target / unknown consult call / dialog gone / send failure. |
| `siphon_ai_outbound_calls_active`       | gauge     | —                                     | In-flight outbound calls (admitted but not yet settled). Compare with `[outbound].max_concurrent`. |
| `siphon_ai_call_duration_seconds`       | histogram | —                                     | Wall-clock duration of ended calls. |
| `siphon_ai_sdp_negotiate_seconds`       | histogram | `result=ok\|error`                    | Time spent in `prepare_call` (negotiate + port alloc + tap attach). |
| `siphon_ai_ws_connect_seconds`          | histogram | —                                     | WS handshake time. |
| `siphon_ai_register_state{name,state}`  | gauge     | `name`, `state=pending\|registered\|auth_failed\|rejected\|failed\|disabled` | Current row per `[[register]]`. Exactly one state per `name` is `1` at any time. |
| `siphon_ai_register_attempts_total`     | counter   | `name`, `outcome=registered\|auth_failed\|rejected\|transport_error` | One tick per REGISTER attempt. |
| `siphon_ai_silence_events_total`        | counter   | —                                     | Times `silence_detected` fired on the WS bridge. Configurable via `[bridge].silence_threshold_ms`. |
| `siphon_ai_dead_air_events_total`       | counter   | —                                     | Times `dead_air_detected` fired on the WS bridge. Configurable via `[bridge].dead_air_threshold_ms`. |
| `siphon_ai_rtp_jitter_ms`               | histogram | —                                     | RTP jitter snapshot recorded on every `rtp_stats` emission (when forge has reported a value). |
| `siphon_ai_rtp_packet_loss_ratio`       | histogram | —                                     | Packet-loss ratio (0.0-1.0) recorded on every `rtp_stats` emission. |
| `siphon_ai_rtp_rtt_ms`                  | histogram | —                                     | RTCP-derived round-trip time (ms) per received Receiver Report (RFC 3550 §A.7). Populated since 0.3.2 (forge originates SRs); explicit buckets 10ms–1s. Records a sample roughly every RTCP cycle (~5s) once bidirectional RTCP is flowing. |
| `siphon_ai_sip_tls_reload_attempts_total` | counter | `outcome=ok\|failed`                  | One tick per SIGHUP cert-reload attempt. `failed` means a broken cert/key on disk; the listener keeps serving the previous cert. |
| `forge_rtcp_*`                          | various   | per-call (forge-side)                 | RTP/RTCP quality. See forge-media's own metric inventory. |
| `heplify_*`                             | various   | from the HEP collector                | Only visible if you scrape heplify too. |

The §11.8 ten-questions audit in `docs/OPERATIONS.md` shows how to use
these alongside logs + traces + HEP to diagnose a problem call without
attaching a debugger.

## Capacity guidance

v1 targets, validated against a single reference node (4 vCPU, 8 GB):

- Steady-state: 500 concurrent calls
- Burst: 50 call setups per second
- Per-call added latency at the bridge: <20 ms p99

Above 500 concurrent calls, scale horizontally — every call's state is owned
by its own task with no cross-call shared state (CLAUDE.md §4.4), so
round-robin or hash-by-Call-ID at L4 fans out trivially across nodes.
Registrations are independent: each node sends its own REGISTER per
configured block.

Soak / burst harnesses live in `test-harness/load/`; see the README there
for the validation procedure used to gate releases.
