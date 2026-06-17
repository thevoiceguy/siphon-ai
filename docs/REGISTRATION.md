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

### Advertised address vs. bind address

A `REGISTER`'s `Via` sent-by and `Contact` host tell the registrar where to
send INVITEs back, so they must be a **reachable** address — never the
wildcard bind. When `[sip].listen` binds an unspecified address
(`0.0.0.0:5060` or `[::]:5060`), you **must** set `[node].public_address` to
the host the registrar can route to; SiphonAI advertises that address
(combined with the listen port) in the REGISTER, while the socket still binds
all interfaces:

```toml
[node]
public_address = "10.246.253.199"   # reachable IP the registrar routes back to

[sip]
listen = "0.0.0.0:5060"             # bind all interfaces
```

This is the same `public_address` used in the SDP `c=` line and the inbound
UAS `Contact`. A wildcard bind without `[node].public_address` is rejected at
config load. A concrete, non-wildcard `[sip].listen` (e.g.
`10.246.253.199:5060`) needs no extra setting — that address is advertised
directly.

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

## TLS registration (0.3.0+)

To register over `sips:` (RFC 3261 §26.2.1), set `transport = "tls"`:

```toml
[[register]]
name      = "carrier-secure"
server    = "203.0.113.42"   # literal IP (hostnames are 0.3.1, see below)
port      = 5061              # default for TLS
transport = "tls"
username  = "siphon"
password  = "${REGISTRAR_PASSWORD}"
```

What this gets you:

- The daemon's `IntegratedUAC` builds a `sips:` URI from `server`/`port`
  and dispatches the REGISTER over the TLS transport that siphon-rs's
  `sip-transport` (W1 PR siphon-rs#49) negotiates. **No fallback to UDP.**
- Trust uses the daemon-wide TLS client roots — currently the
  `webpki-roots` Mozilla CA bundle baked into the binary. Same trust
  store the bridge WS leg uses by default (see
  [`docs/DEPLOY.md`](DEPLOY.md) §3a).
- mTLS: if your carrier requires a client cert for SIP-side TLS, the
  daemon presents the same `[sip.tls]` cert it uses for inbound — there
  is no separate `[[register]].tls` client cert in 0.3.0.

### Twilio Elastic SIP Trunk example

Twilio's secure SIP runs on `:5061` over TLS; bind via the regional
edge IP returned by their portal (the public hostname uses anycast,
so picking one IP at config time keeps the registrar stable):

```toml
[[register]]
name      = "twilio-secure"
server    = "54.172.60.0"
port      = 5061
transport = "tls"
username  = "your-trunk-username"
password  = "${TWILIO_TRUNK_PASSWORD}"
```

## What 0.3.0 still does NOT support

- **DNS-resolved registrar hostnames.** Configs still accept literal
  IPv4 / IPv6 addresses only; hostnames produce a clear error at
  config-load time. The runtime resolver in `sip-dns` exists but the
  static IP requirement keeps config validation simple and
  startup-deterministic. SRV/NAPTR-driven failover (RFC 3263
  `_sips._tcp`) and hostname registrars are 0.3.1+.
- **SIGHUP reload of registrations.** The set is static for the daemon's
  lifetime; restart to add/remove/edit a `[[register]]` block.
- **Per-registration cert pinning.** All TLS registrations share the
  daemon-wide webpki trust store. SPKI pinning on a per-`[[register]]`
  basis (the `[[register]].tls.pinned_sha256` shape the dev plan
  sketches) needs an siphon-rs API to install a per-target
  `ClientConfig` and isn't done. 0.3.1+ follow-up.

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
