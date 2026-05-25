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

`siphon-ai` re-reads cert + key on startup, not at runtime. The
simplest reliable pattern is to restart on renewal:

```sh
# Let's Encrypt deploy-hook (drop into /etc/letsencrypt/renewal-hooks/deploy/)
#!/bin/sh
set -e
install -m 0640 -o root -g siphon \
    "$RENEWED_LINEAGE/fullchain.pem" /etc/siphon-ai/tls/
install -m 0640 -o root -g siphon \
    "$RENEWED_LINEAGE/privkey.pem"   /etc/siphon-ai/tls/
systemctl restart siphon-ai
```

A restart drops in-flight calls. If your traffic pattern can't
tolerate that, run two instances behind an L4 load balancer and
restart them one at a time. Hot cert reload is on the roadmap for
a later release.

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

The webhook sink delivers the same JSON to `[cdr.webhook].url` with
`Content-Type: application/json`. Retries on non-2xx up to
`[cdr.webhook].retry_max` times with exponential backoff.

## Lifecycle webhooks

Off-band events (NOT the per-call WS bridge). Same retry semantics as the
CDR webhook. Event types:

| `type`                          | When                                             |
|---------------------------------|--------------------------------------------------|
| `call_start`                    | After 200 OK has gone out on an accepted INVITE. |
| `call_end`                      | After the controller exits and the CDR record is built. |
| `registration_state_changed`    | Each `[[register]]` state transition (`pending` → `registered`, `registered` → `failed`, etc.). |

Each delivery is a single JSON object with `version`, `timestamp` (ISO 8601), `type`,
and per-event fields documented in `crates/webhooks/src/event.rs`.

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
| `siphon_ai_invites_total`               | counter   | `result=accepted\|rejected`           | INVITEs by acceptance outcome. |
| `siphon_ai_calls_total`                 | counter   | `cause=server_hangup\|local_shutdown\|bridge_ended\|tap_ended` | Ended calls by termination cause. |
| `siphon_ai_calls_active`                | gauge     | —                                     | Currently-running calls. |
| `siphon_ai_route_match_total`           | counter   | `route`                               | Calls per matched route. |
| `siphon_ai_call_duration_seconds`       | histogram | —                                     | Wall-clock duration of ended calls. |
| `siphon_ai_sdp_negotiate_seconds`       | histogram | `result=ok\|error`                    | Time spent in `prepare_call` (negotiate + port alloc + tap attach). |
| `siphon_ai_ws_connect_seconds`          | histogram | —                                     | WS handshake time. |
| `siphon_ai_register_state{name,state}`  | gauge     | `name`, `state=pending\|registered\|auth_failed\|rejected\|failed\|disabled` | Current row per `[[register]]`. Exactly one state per `name` is `1` at any time. |
| `siphon_ai_register_attempts_total`     | counter   | `name`, `outcome=registered\|auth_failed\|rejected\|transport_error` | One tick per REGISTER attempt. |
| `siphon_ai_silence_events_total`        | counter   | —                                     | Times `silence_detected` fired on the WS bridge. Configurable via `[bridge].silence_threshold_ms`. |
| `siphon_ai_dead_air_events_total`       | counter   | —                                     | Times `dead_air_detected` fired on the WS bridge. Configurable via `[bridge].dead_air_threshold_ms`. |
| `siphon_ai_rtp_jitter_ms`               | histogram | —                                     | RTP jitter snapshot recorded on every `rtp_stats` emission (when forge has reported a value). |
| `siphon_ai_rtp_packet_loss_ratio`       | histogram | —                                     | Packet-loss ratio (0.0-1.0) recorded on every `rtp_stats` emission. |
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
