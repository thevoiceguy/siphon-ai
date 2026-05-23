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
