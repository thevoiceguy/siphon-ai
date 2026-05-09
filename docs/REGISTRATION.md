# Registering SiphonAI as a Phone

In **registered phone mode**, SiphonAI sends an outbound `REGISTER` to a
PBX/SBC and is treated as a normal SIP endpoint. Calls the PBX routes to
SiphonAI's AOR arrive as inbound `INVITE`s, get matched against `[[route]]`
entries (using `register_source` to identify the registration the call came
in on), and bridge to your WS server like any other inbound call.

The alternative is **trunk mode (UAS)**: SiphonAI just listens on
`[sip].listen` and the PBX/trunk is configured with SiphonAI as a SIP target.
You can run both in the same daemon — registrations and trunk listeners
coexist.

## Minimal config

```toml
[sip]
listen = "0.0.0.0:5060"
transports = ["udp"]

[[register]]
name = "cucm-main"
server = "10.10.0.50"        # registrar IP (v1: literal IPv4 only)
port = 5060                  # default 5060 / 5061 by transport
transport = "udp"            # udp | tcp | tls
username = "ai-receptionist" # AOR user-part
auth_username = "ai-receptionist"   # defaults to `username`
password = "${SIP_PASSWORD_CUCM}"   # env-expanded
realm = "example.com"        # optional; informational
expires_secs = 3600          # default 3600
register_on_startup = true   # default true

[[route]]
name = "from-cucm"
[route.match]
register_source = "cucm-main"   # matches calls that arrived on this registration
[route.bridge]
ws_url = "wss://reception.example.com/sip-bridge"
```

`register_source = "cucm-main"` matches inbound INVITEs whose source peer
address equals the registered server's address. Inbound calls that don't
arrive on a registered transport get the implicit source `"trunk"`.

You can declare any number of `[[register]]` blocks; each runs an independent
drive task. Names must be unique.

## What v1 supports

- Initial REGISTER + Digest auth retry (handled by the upstream
  `IntegratedUAC::register`).
- Periodic refresh at `expires - 60s` (with a 5-second floor).
- Exponential backoff on failure: 5s → 10s → 20s → … capped at 300s.
- Per-registration `RegistrationState` snapshot for ops tooling.
- Routing: an inbound INVITE's peer address resolves to the registration's
  `name`, which is what `register_source` matches in `[[route]]` blocks.
- Lifecycle webhook (`registration_state_changed`) on every transition.
- Prometheus metrics (`siphon_ai_register_state`, `siphon_ai_register_attempts_total`).

## What v1 does NOT support (yet)

- DNS-resolved registrar hostnames. v1 accepts literal IPv4 addresses only;
  hostnames produce a clear error at config-load time. SRV/NAPTR-driven
  failover lives in v1.1.
- SIGHUP reload of registrations. The set is static for the daemon's
  lifetime; restart to add/remove/edit a `[[register]]` block.
- Per-registration TLS client roots. The daemon's webpki trust store applies
  to all TLS registrations.
- Outbound TCP/TLS connect from the UAC for registrations on those
  transports. v1 sends REGISTER over UDP regardless of `transport = "tcp"|"tls"`.

## Observability

### Prometheus

Two metrics are exported (see `docs/DEPLOY.md` for the full list):

```
# HELP siphon_ai_register_state Per-[[register]] status.
# TYPE siphon_ai_register_state gauge
siphon_ai_register_state{name="cucm-main",state="registered"} 1
siphon_ai_register_state{name="cucm-main",state="pending"} 0
siphon_ai_register_state{name="cucm-main",state="failed"} 0
siphon_ai_register_state{name="cucm-main",state="disabled"} 0

# HELP siphon_ai_register_attempts_total REGISTER attempts by [[register]].name and outcome.
# TYPE siphon_ai_register_attempts_total counter
siphon_ai_register_attempts_total{name="cucm-main",outcome="registered"} 12
siphon_ai_register_attempts_total{name="cucm-main",outcome="auth_failed"} 0
siphon_ai_register_attempts_total{name="cucm-main",outcome="rejected"} 0
siphon_ai_register_attempts_total{name="cucm-main",outcome="transport_error"} 1
```

Page on `siphon_ai_register_state{state="failed"} == 1` for any name where it
matters operationally. The `outcome` label values are stable strings — see
`crates/telemetry/src/metrics.rs` for the canonical list.

### Webhook

When `[webhooks].enabled = true` and the event allowlist includes
`registration_state_changed` (or no allowlist is set), every status
transition produces:

```json
{
  "type": "registration_state_changed",
  "version": 1,
  "name": "cucm-main",
  "timestamp": "2026-05-09T15:46:53.069Z",
  "status": "registered",
  "previous_status": "pending",
  "last_error": null,
  "expires_at": "2026-05-09T16:16:53.069Z"
}
```

`previous_status` is `null` only on the very first emit after process start.
On `registered → failed` transitions, `last_error` carries a free-form
description (`"401 Unauthorized"`, `"timeout"`, `"connection refused"`).

### Logs

Each registration runs in its own `instrument`ed span:

```
INFO drive{name=cucm-main server=10.10.0.50:5060}: registration drive started
INFO drive{name=cucm-main server=10.10.0.50:5060}: registration succeeded granted_expires_secs=3540
WARN drive{name=cucm-main server=10.10.0.50:5060}: registration failed; will retry after backoff outcome="auth_failed" error="401 Unauthorized" backoff_secs=5
```

Filter with `RUST_LOG=siphon_ai::registration=debug` for the per-attempt
detail (sleep deltas, transition events).

## Vendor notes

### Asterisk (PJSIP)

Tested against Asterisk 20.x with a `pjsip.conf` endpoint:

```ini
[ai-receptionist](endpoint-template)
context = from-internal
disallow = all
allow = ulaw,alaw
auth = ai-receptionist-auth
aors = ai-receptionist

[ai-receptionist-auth](auth_userpass)
username = ai-receptionist
password = <set this to match SIP_PASSWORD_CUCM>

[ai-receptionist](aor)
max_contacts = 1
```

Asterisk's `Expires` defaults to 3600 in 200 OK. No surprises.

### Cisco CUCM

Treats SiphonAI as a "Third-Party SIP Device (Advanced)". Gotchas:

- CUCM expects a non-empty `User-Agent`. We send `siphon-rs/<version>`.
- CUCM may include a `Service-Route` header in 200 OK; the upstream UAC
  honors it transparently.
- Some CUCM deployments require digest auth even for the OPTIONS keep-alive
  flow. v1 doesn't send keep-alive OPTIONS — the registrar's `Expires` is
  the source of truth for liveness.

### FreeSWITCH

Treats us as a regular gateway endpoint. Set the gateway's `username` to the
`[[register]].username` and `password` to the `[[register]].password`.

## Troubleshooting

| Symptom | Likely cause | Check |
|---|---|---|
| `siphon_ai_register_state{state="pending"}` stays at 1 forever | First REGISTER never gets a response | Packet capture on the registrar's IP; firewall on `[sip].listen` |
| `outcome="transport_error"` and the error is `Timer F expired` | Daemon sent REGISTER but no response arrived in 32s | Verify the registrar received it (PBX trace logs); confirm response source IP/port matches the daemon's `[sip].listen` |
| `outcome="auth_failed"` after one attempt | Wrong `password` or `realm` | Compare the digest challenge details in tracing (`RUST_LOG=sip_auth=debug`) against the PBX config |
| Refresh hits `transport_error` after a long-running success | Registrar restarted and lost our binding; or NAT mapping expired | The exponential backoff will retry; if it persists, shorten `expires_secs` so refresh runs more often |
| Inbound INVITEs from the PBX get `register_source = "trunk"` instead of the registration name | Source IP/port doesn't match what `[[register]].server`/`port` resolved to | Confirm the PBX sends INVITEs from the same address it accepts REGISTERs on |
