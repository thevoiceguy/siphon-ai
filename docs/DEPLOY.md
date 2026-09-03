# Deployment Guide

This is the operator's reference for running `siphon-ai` in something other
than `cargo run`. For configuration semantics see `docs/CONFIG.md`; for the
observability bar (the §11.8 ten questions) see `docs/OPERATIONS.md`.

## Install from a release

Every [GitHub release](https://github.com/thevoiceguy/siphon-ai/releases)
ships prebuilt, **statically-linked musl** artifacts for `x86_64` and
`aarch64` — no need to build from source. Each release carries:

| Artifact | What it is |
|---|---|
| `siphon-ai-<ver>-<target>.tar.gz` | Standalone static binary (+ licenses, README). |
| `siphon-ai_<ver>-1_<arch>.deb` | Debian/Ubuntu package (`amd64` / `arm64`). |
| `ghcr.io/thevoiceguy/siphon-ai:<ver>` | Multi-arch container (`linux/amd64` + `linux/arm64`). |
| `siphon-ai-<ver>-sbom.cdx.json` | CycloneDX SBOM. |
| `SHA256SUMS`, `SHA256SUMS.cosign.bundle` | Checksums + a keyless [cosign](https://docs.sigstore.dev/) signature over them. |

The binaries are the same artifacts the container is built from — byte-for-byte
per arch.

### Verify before installing

```sh
# Integrity: checksums cover the tarballs, the .deb packages, and the SBOM.
sha256sum -c SHA256SUMS

# Provenance: the cosign bundle is signed by the release workflow's GitHub
# OIDC identity (keyless — no public key to distribute).
cosign verify-blob --bundle SHA256SUMS.cosign.bundle \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com \
  --certificate-identity-regexp 'release\.yml@refs/tags/' \
  SHA256SUMS
```

### Binary tarball

```sh
tar xzf siphon-ai-<ver>-x86_64-unknown-linux-musl.tar.gz
sudo install -m255 siphon-ai-<ver>-*/siphon-ai /usr/local/bin/siphon-ai
siphon-ai --version
```

### Debian / Ubuntu package

```sh
sudo apt install ./siphon-ai_<ver>-1_amd64.deb   # resolves ca-certificates + adduser
```

The package installs the binary to `/usr/bin/siphon-ai`, a default config to
`/etc/siphon-ai/config.toml` (a dpkg *conffile* — your edits survive
upgrades), and a hardened systemd unit. It creates the `siphon-ai` service
user and `/var/{lib,log}/siphon-ai`, and **enables but does not start** the
service — the default config points at a placeholder `[bridge].ws_url`. Edit
the config, then `sudo systemctl start siphon-ai`. (`systemctl reload`
hot-applies route/gateway/webhook changes via `SIGHUP`.)

### Container

```sh
docker pull ghcr.io/thevoiceguy/siphon-ai:<ver>
# Verify the image signature (same keyless identity as the blob above):
cosign verify ghcr.io/thevoiceguy/siphon-ai:<ver> \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com \
  --certificate-identity-regexp 'release\.yml@refs/tags/'

docker run --rm -v "$PWD/config.toml:/app/config.toml" \
  ghcr.io/thevoiceguy/siphon-ai:<ver>
```

To build the container yourself instead, see **Container image** below.

## Build prerequisites

The daemon links **libopus** (Opus codec support, 0.8.0) — built from source
by the `audiopus` crate at compile time. Building `siphon-ai` therefore needs
a **C toolchain + CMake** (`cc`/`g++`, `make`, `cmake`, `perl`) on the build
host, in addition to the Rust toolchain. The shipped `docker/Dockerfile`
builder stage already installs these (musl-dev, cmake, make, g++, pkgconfig,
perl), so `docker build` is turnkey. For a bare `cargo build`, install the
equivalents (e.g. `apt install build-essential cmake` /
`apk add musl-dev cmake make g++`). The runtime image needs nothing extra —
libopus is statically linked into the binary.

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
| `[media].rtp_port_range` | UDP | TOML       | both      | RTP/RTCP. Forge allocates one even-numbered RTP port + the next odd RTCP port per call. Forward the whole range. **Also reserve it from the kernel — see below.** |
| `[observability].http_listen` | TCP | TOML  | inbound (cluster-local) | `/metrics`, `/health`, `/ready`. Unauthenticated — keep it cluster-local. Since 0.10.0 `/admin/*` is **not** served here (returns `404`); it moved to the dedicated `[admin]` listener below. |
| `[admin].listen`  | TCP    | TOML           | inbound (cluster-local) | `/admin/*` control plane (0.10.0). Bearer-token auth + RBAC (`readonly` ⊂ `operator` ⊂ `admin`); omit `[admin]` and `/admin/*` isn't served at all. Set `[admin.tls]` (0.18.0) to serve HTTPS so the token is encrypted on a routable bind. Still keep it off the public internet. |
| Outbound, dynamic | TCP    | `[bridge].ws_url` (per route) | outbound | WebSocket from daemon to operator's WS server. |
| Outbound, dynamic | TCP    | `[cdr.webhook].url`, `[webhooks].url` | outbound | HTTP POSTs for CDRs and lifecycle webhooks. |
| Outbound 9060     | UDP    | `[hep].collector` | outbound | HEP3 to Homer. UDP only in v1. |
| Outbound 5060/5061 | UDP/TCP | `[[register]].server` | bidirectional | Per `[[register]]` block. |

### Reserve the RTP range from the kernel

Forwarding the RTP range is not enough. On Linux the range you pick almost
certainly sits **inside** the ephemeral port range the kernel hands out to
any socket that does not bind an explicit port:

```sh
$ cat /proc/sys/net/ipv4/ip_local_port_range
32768   60999
```

A typical `rtp_port_range = [40000, 40500]` is entirely within it. Nothing
stops the kernel from giving one of those ports to an unrelated UDP socket
— **a DNS lookup by any process on the host is enough** — and when a call
then needs that port, the bind fails and the INVITE is rejected:

```
WARN rejecting INVITE error=forge session error: Network error:
  Failed to bind socket to 0.0.0.0:44134: Address in use (os error 98)
  code=500 reason="Server Internal Error"
```

The call is lost, and the daemon looks at fault when the host is. It is
rare, it is silent until it happens, and it scales with how much ephemeral
UDP traffic the host generates rather than with call volume — so a busy
host drops calls a quiet one never will. Measured once in 399 calls during
a 200-concurrent soak (issue #504).

Reserve the range so the kernel never issues it ephemerally — match your
`rtp_port_range` exactly:

```sh
# /etc/sysctl.d/60-siphon-ai-rtp.conf
net.ipv4.ip_local_reserved_ports = 40000-40500
```

```sh
sudo sysctl --system
cat /proc/sys/net/ipv4/ip_local_reserved_ports   # verify: 40000-40500
```

Reserving costs nothing — the ports are already yours in intent, and the
kernel simply stops handing them to anyone else. The alternative is to
choose an `rtp_port_range` above `ip_local_port_range`'s ceiling, which
works equally well but leaves less room to grow.

### Sizing the pool on a node that also originates

**One pool serves both directions.** An originated leg holds a port pair
exactly like an accepted one, so `rtp_port_range` must be sized for the
**sum** of inbound and outbound concurrency plus headroom — two ports per
concurrent call, whichever way it is going.

By default the two directions are **unreserved and first-come-first-served**,
which is not how an operator would prioritise them. Measured on 0.49.9 with
the pool shrunk to 60 calls and 50 inbound + 20 outbound asked for
(`test-harness/load/RESULTS-0.49.9-mixed-and-soak.md` §2), inbound
established **50/50 and stayed healthy for the whole window** while
**10 of 20 originates failed** — the direction that usually has a deadline
attached absorbed the entire shortfall, and nothing in the inbound metrics,
logs, or CDRs said so.

Two things follow:

- **`[media].reserved_outbound_calls = N`** holds `N` pairs back from the
  inbound allocator (0.50.0, [#556]). Inbound is refused `503` +
  `Retry-After` once free pairs reach `N`; origination is not gated and may
  use them. Sizing: set it to your peak concurrent originations. The floor is
  exact — evaluated inside the pool allocator's own critical section, so
  concurrent INVITE setup cannot dip below it and no slack is needed. It is
  **not** a substitute for sizing the range; it decides who loses when the
  range is too small.
- **Watch the failure from the outbound side.** `POST /admin/v1/calls`
  returns `202` and a port-pool failure arrives later on the
  `outbound_failed` webhook, so an HTTP status code will never show it.
  The signals are `siphon_ai_outbound_calls_total{result="failed"}` and
  that webhook. On the inbound side,
  `siphon_ai_invites_total{result="rejected_capacity"}` covers both an
  exhausted pool and a reservation refusal;
  `siphon_ai_rtp_reserve_blocks_total` isolates the latter.

[#556]: https://github.com/thevoiceguy/siphon-ai/issues/556

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

#### Mutual TLS (0.51.0)

To authenticate the *peer* rather than only encrypt to it — a
cascade of conference nodes, a partner trunk that must not be
reachable by anyone who found the port — have the listener ask for a
client certificate and verify it against your CA:

```toml
[sip.tls]
listen      = "0.0.0.0:5061"
cert        = "/etc/siphon-ai/tls/fullchain.pem"
key         = "/etc/siphon-ai/tls/privkey.pem"
client_ca   = "/etc/siphon-ai/tls/trunk-ca.pem"   # what peers' certs must chain to
client_auth = "required"                          # or "optional" while rolling out
```

`required` refuses the handshake — the peer never sends a SIP
message. `optional` admits a peer with no certificate but still
refuses one whose certificate does not chain; its INVITEs then carry
no identity. Either way an INVITE that arrives on a verified
connection carries the certificate's subject and SANs, which the
`peer_cert_san` route key matches on (`docs/DIALPLAN.md` §4.6), the
`INVITE arrived on a connection with a verified client certificate`
log line prints with the SHA-256 fingerprint, and
`GET /admin/v1/calls` shows as `peer_identity`. Roll out with
`optional` and watch `siphon_ai_tls_peer_identity_total{result="none"}`
drop to zero before switching to `required`.

For the reverse direction — a trunk that demands a certificate from
*us* — set `[sip.tls_client].client_cert` + `client_key`; every
outgoing TLS connection presents it to a peer that asks. The key must
be `0600` like the listener key. `siphon-ai check` validates both
blocks (paths exist, halves come in pairs, mode is
`optional|required`) before a restart.

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

The systemd unit runs as the unprivileged `siphon-ai` user (created
by the `.deb`). The cert chain is public — root-owned and
group-readable is right. The **private key is different**: the daemon
refuses to start if the key has any group- or world-readable bits
("insecure permissions" error), so it must be `0600` — and therefore
*owned by* `siphon-ai`, or the daemon can't read it at all.

```sh
sudo install -d -m 0750 -o root -g siphon-ai /etc/siphon-ai/tls
sudo install -m 0640 -o root -g siphon-ai fullchain.pem /etc/siphon-ai/tls/
sudo install -m 0600 -o siphon-ai -g siphon-ai privkey.pem /etc/siphon-ai/tls/
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
reload siphon-ai` to the SIGHUP. Since 0.12.0 the same `SIGHUP`
also reloads the **config file** — see [Config file
reload](#config-file-reload-0120) below.

```sh
# Let's Encrypt deploy-hook (/etc/letsencrypt/renewal-hooks/deploy/)
#!/bin/sh
set -e
install -m 0640 -o root -g siphon-ai \
    "$RENEWED_LINEAGE/fullchain.pem" /etc/siphon-ai/tls/
install -m 0600 -o siphon-ai -g siphon-ai \
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

#### Config file reload (0.12.0)

The same `SIGHUP` (`systemctl reload siphon-ai`) also re-reads the
`--config` file and **hot-applies the reload-safe sections without
dropping calls**:

- **routes** — new INVITEs use the new dialplan; in-flight calls keep the
  route they matched;
- **`[webhooks]` + `[cdr]` sinks** — rebuilt and swapped, *unless* a
  durable `spool_dir` is active for that sink (its drain worker can't be
  hot-swapped → restart required for delivery changes there);
- **outbound gateways** (`[[gateway]]`, 0.12.1) — the set is rebuilt +
  swapped (add / remove / modify trunks, **including rotating a gateway's
  `auth_password`**); in-flight outbound calls keep the trunk they're on.
  Needs outbound enabled and the `[outbound]` limits unchanged;
- the `[sip.tls]` cert (above).

**Always `check` before you reload** — a reload is exactly as safe as the
config you feed it:

```sh
siphon-ai check --config /etc/siphon-ai/config.toml && systemctl reload siphon-ai
```

**Fail-safe.** A config that doesn't load/compile is logged, the running
config is **kept**, and `siphon_ai_config_reloads_total{result="failed"}`
ticks — a bad edit can't take the daemon down (same posture as the cert
reload). On success the counter ticks `applied`, or `no_change` when the
file is byte-identical to the last load.

**Restart-required sections.** Everything consumed only at startup —
`[node]`, `[sip]`, `[media]`, the `[bridge]`/codec defaults, `[[trunk]]`,
`[[register]]`, `[security]` (incl. `min_attestation`), `[recording]`,
`[conference]`, `[park]`, `[observability]`, `[admin]` (incl. the token
table — a rotated/revoked admin token keeps working **until restart**),
`[hep]`, and the `[outbound]` limits (`max_concurrent` / `rate_limit_per_sec`,
which also flip outbound on/off) — needs a process **restart**. A reload that
changes one of these applies the safe sections and logs a `warn!` naming the
section(s) that did not take effect (it is never silently swallowed) — grep
the journal for `require a restart` after a reload to catch this.

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
User=siphon-ai
Group=siphon-ai
EnvironmentFile=-/etc/siphon-ai/env
ExecStart=/usr/local/bin/siphon-ai --config /etc/siphon-ai/siphon-ai.toml
# SIGHUP triggers SIP/TLS cert hot-reload (0.3.0+). `systemctl
# reload siphon-ai` invokes this — see §5 above for renewal flow.
ExecReload=/bin/kill -HUP $MAINPID
# On stop, systemd sends SIGTERM; the daemon drains active calls
# (0.17.0, [shutdown].drain_timeout_secs). Give it longer than that
# window + a couple seconds of BYE-flush grace, or systemd SIGKILLs
# mid-drain. Default drain is 30 s → 40 s here. A second SIGTERM
# (systemctl stop twice) forces an immediate exit.
TimeoutStopSec=40
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

## Graceful shutdown & rolling deploys

On `SIGTERM` / `SIGINT` the daemon **drains** before exiting (0.17.0,
configured by [`[shutdown]`](CONFIG.md#shutdown)): it flips `/ready` to
not-ready, rejects **new** inbound INVITEs with `503 Service Unavailable`
+ `Retry-After`, lets in-flight calls finish (bounded by
`drain_timeout_secs`, default 30 s), then force-terminates any stragglers
at the deadline with a real `BYE` + WS `hangup`. This is what makes a
zero-drop rolling restart possible. The two signals to the outside world
are complementary:

- **`/ready` → 503** tells a load balancer / k8s readiness gate to stop
  sending *new* calls here — but it only notices on its next poll.
- **`503` on new INVITEs** covers the gap until then: anything an upstream
  SIP proxy routes to this node mid-drain is rejected with a retryable
  code so it fails over immediately.

In-dialog requests (re-INVITE for hold/resume, ACK, BYE) for calls already
up keep flowing, so the calls being drained aren't broken.

**The one rule:** `drain_timeout_secs` **must be ≤** your supervisor's
kill grace, or the supervisor `SIGKILL`s the daemon mid-drain and you lose
the very calls you were protecting:

- **systemd:** `TimeoutStopSec` (see the unit sketch above — set it a few
  seconds *above* the drain window for the BYE-flush grace).
- **Kubernetes:** `terminationGracePeriodSeconds` on the pod spec.

### Kubernetes

```yaml
spec:
  terminationGracePeriodSeconds: 40   # ≥ drain_timeout_secs + a few s
  containers:
    - name: siphon-ai
      readinessProbe:
        httpGet: { path: /ready, port: 9091 }
        periodSeconds: 2
      # No preStop hook needed: k8s sends SIGTERM directly and the daemon
      # drains on it. (A `preStop: sleep` is only useful if your daemon
      # ignored SIGTERM — this one doesn't.)
```

k8s removes the pod from Service endpoints and sends `SIGTERM` at the same
time; the `503` reject path bridges the brief window before Endpoints
propagation completes. Watch the drain with the `siphon_ai_draining` gauge
(1 while draining), `siphon_ai_drain_seconds` (how long it took), and
`siphon_ai_calls_drain_forced_total` (calls that didn't finish in the
window — if this is regularly non-zero, raise `drain_timeout_secs` and the
grace). `GET /admin/v1/drain` gives the live `{draining, active_calls,
remaining_secs}` snapshot during a drain.

Set `drain_timeout_secs = 0` to opt out and keep the pre-0.17.0 behaviour
(immediate teardown, active calls dropped). A **second** SIGTERM/SIGINT
during a drain forces an immediate exit (operator escape hatch).

## Admin API

`/admin/*` is served **only on the dedicated `[admin]` listener**, gated by
a bearer token + RBAC (0.10.0). It is **no longer** on the
`[observability].http_listen` port — that port now serves only `/metrics`,
`/health`, `/ready`, and returns `404` for `/admin/*`. Set up the listener
and tokens first ([Admin auth & RBAC](#admin-auth--rbac) below); every
example in this section assumes the admin port and an
`Authorization: Bearer …` header.

The **Min role** column is the lowest role a token needs to reach the
endpoint (roles nest: `readonly` ⊂ `operator` ⊂ `admin`). Below it → `403`;
no/invalid token → `401`.

All endpoints live under `/admin/v1/` (canonical since 0.43.0, issue #362).
The five routes that predate 0.6.0 are **also** served at their original
unversioned paths as deprecated aliases — see
[Deprecated unversioned aliases](#deprecated-unversioned-aliases) below.

| Method | Path                          | Body            | Min role | Purpose |
|--------|-------------------------------|-----------------|----------|---------|
| GET    | `/admin/v1/calls`             | —               | readonly | List active calls (inbound **and** outbound). Each element is `{call_id, sip_call_id, direction}`, plus **`webrtc_state`** on a browser call (`DEV_PLAN_WebRTC.md` §4.6): `"connecting"` (ICE checks running — no path yet), `"ice_connected"` (a pair is nominated and DTLS is handshaking: a call **stuck** here has a working path and failing crypto), `"connected"` (SRTP keys installed, media flowing), `"failed"`, `"closed"`. The key is **absent for a classic SIP leg**, which has no such phase — absent means "not a browser call", never "unknown". Otherwise — the **bridge** `call_id` (what every other `/admin/v1/calls/:id/…` route and the conference endpoints take, and the value on the WS `start` message / CDR), the **SIP** Call-ID (what the deprecated `hangup` alias takes), and `"inbound"`/`"outbound"`. Since 0.37.1; before that it returned a bare array of SIP Call-ID strings and exposed no way to obtain the bridge id (issue #311). |
| POST   | `/admin/v1/calls/:id/hangup`  | —               | operator | Force-shutdown a specific call by its **bridge** `call_id` (0.43.0) — the same id `/park`, `/retrieve` and `/stats` take. Works for inbound and outbound calls; the daemon BYEs the peer and tears the call down. `200 {shutdown_signalled: true, call_id, sip_call_id}`; `404` when no active call has that id. |
| GET    | `/admin/v1/registrations`     | —               | readonly | Snapshot of every `[[register]]` row and its current state. |
| POST   | `/admin/v1/registrations/:name/refresh` | —     | operator | **Fire an immediate off-cycle REGISTER** for one binding (0.33.0) — no restart needed when a registrar drops or stales the binding. Also **starts a parked binding** (`register_on_startup = false`). Returns `202` with the accept-time row; the outcome is asynchronous (watch the GET, `siphon_ai_register_attempts_total`, or the `registration_state_changed` webhook). `404` unknown name, `409` while draining. |
| POST   | `/admin/v1/registrations/:name/restart` | —     | operator | **Full re-registration cycle** (0.33.0): REGISTER `Expires: 0` to clear the registrar-side binding, then a fresh REGISTER — for stale server-side state or contact rebinding after an IP change. A failed unregister is logged and the fresh REGISTER proceeds; only the final attempt drives status. Same responses as `refresh`. |
| GET    | `/admin/v1/log`               | —               | readonly | Current `tracing` filter directive. |
| PUT    | `/admin/v1/log`               | text directive  | admin    | Replace the filter (e.g., `siphon_ai=info,siphon_ai_bridge=debug`). Returns the previous filter. |
| POST   | `/admin/v1/hep/test`          | —               | admin    | Emit a probe HEP log packet. |
| POST   | `/admin/v1/calls`             | JSON (below)    | admin    | **Originate an outbound call** (0.6.0). Returns `202 {"call_id": "..."}`; the call proceeds asynchronously. `501` when `[outbound]` is disabled. |
| GET    | `/admin/v1/conferences`       | —               | readonly | **List conference rooms** + members (0.7.0). `501` when `[conference]` is disabled. |
| POST   | `/admin/v1/conferences`       | JSON (opt.)     | operator | **Pre-create a room** (0.7.0). Body `{room_id?, sample_rate?}`; returns `201 {"room_id": "..."}`. |
| DELETE | `/admin/v1/conferences/:id`   | —               | operator | **Force-end a room** (0.7.0). Every member reverts to its direct pair (`conference_left { room_closed }`). `404` if unknown. |
| POST   | `/admin/v1/conferences/:id/participants`          | JSON | operator | **Add a call to a room** (0.7.0). Body `{call_id}`; `202` (dispatched). |
| DELETE | `/admin/v1/conferences/:id/participants/:call_id` | —    | operator | **Remove a call from a room** (0.7.0). `202` (dispatched). |
| GET    | `/admin/v1/parked`            | —               | readonly | **List parked calls** (0.7.0). `501` when `[park]` is disabled. |
| POST   | `/admin/v1/calls/:id/park`    | JSON (opt.)     | operator | **Park an active call** (0.7.0). Body `{slot?}`; `202` (dispatched). `404` if no active call has that id. `501` when `[park]` is disabled. |
| POST   | `/admin/v1/calls/:id/retrieve`| JSON (opt.)     | operator | **Retrieve a parked call** (0.7.0). Body `{ws_url?}`; `202` (dispatched). `404` unknown call, `409` if the call isn't parked. `501` when `[park]` is disabled. |
| GET    | `/admin/v1/calls/:id/stats`   | —               | readonly | **Live per-call quality snapshot** (0.31.0). `:id` is the bridge `call_id` (the one on the WS `start` message / CDR). Returns `{call_id, sampled_at, …}` with the CDR `quality` block's fields flattened in — whatever is measured *right now*, unmeasured fields omitted. A browser call also carries **`webrtc_state`**, its live ICE/DTLS phase, read at sample time (same vocabulary as `GET /admin/v1/calls`; absent for a classic leg). `404` when no active call has that id (ended calls answer through the CDR / `[quality]` records). |
| GET    | `/admin/v1/calls/:id/sip`     | —               | **operator** | **Per-call SIP ladder** (DESIGN_SIP_LADDER.md). The messages captured for one call, **oldest first**: `{call_id, sip_call_id, truncated, count, messages: [{ts_ms, direction, src, dst, payload}]}`. `:id` is the bridge `call_id`, resolved through the live registry and then the recent-CDR ring — so a call that just *ended* is still inspectable, which is when you usually want it. `direction` is `in`/`out`/`unknown` (matched on IP against `[node].public_address`, a non-wildcard bind, and the unspecified address a wildcard bind stamps; the SIP bind port breaks a loopback tie. `src`/`dst` are kept so the derivation stays checkable). `501` when `[observability].sip_ring_size = 0`; `404` for an unknown id; `200` with an empty list when the call is known but its trace was never captured or has been evicted. **Note the role: this is the only `GET` on this API above `readonly`**, because `payload` is the raw message including `Authorization` / `Proxy-Authorization` — see *Admin auth & RBAC* below. A nice-to-have for a quick look, not a Homer replacement. |
| GET    | `/admin/v1/drain`             | —               | readonly | **Graceful-shutdown drain status** (0.17.0). Returns `{draining, active_calls, drain_timeout_secs, remaining_secs}` — `remaining_secs` is the countdown to the deadline (non-null only while draining). Lets a deploy script confirm a pod entered drain and watch it empty. |
| GET    | `/admin/v1/errors`            | —               | readonly | **Recent-errors ring** (0.49.0). The last N `warn!`/`error!` tracing events, newest first: `{count, errors: [{ts_ms, level, target, message, call_id?}]}` — `call_id` present when the event fired inside a per-call span, so entries join against `/admin/v1/calls`, the CDR, and Homer. Capacity via `[observability].error_ring_size` (default 256; `0` ⇒ always-empty list). Feeds sightglass's Errors tab; equally useful as `curl … /admin/v1/errors \| jq` during an incident. |
| GET    | `/admin/v1/status`            | —               | readonly | **Node status summary** (0.49.0). One-request JSON snapshot: `{version, uptime_secs, active_calls, registrations: {registered, total}, draining, hep_enabled}`. The *live* view only — cumulative counters (`siphon_ai_calls_total`, HEP delivery/health) stay on `/metrics`. Feeds sightglass's overview grid; handy for deploy scripts that would otherwise parse Prometheus text for one number. |
| POST   | `/admin/v1/drain`             | —               | admin    | **Start a graceful drain** (0.49.0) — the programmatic twin of SIGTERM: new INVITEs 503, active calls finish (or are force-terminated at `[shutdown].drain_timeout_secs`), then the daemon exits. `202 {drain_signalled, already_draining}`; idempotent, and — unlike a second SIGTERM — repeating it never forces immediate teardown. Watch progress on `GET /admin/v1/drain`. |
| GET    | `/admin/v1/cdrs/recent`       | —               | readonly | **Recent completed-call CDRs** (0.49.0). The last N calls' CDR records verbatim (`{count, cdrs: […]}`, newest first) — the same schema `[cdr]` sinks write, versioned by each record's `version` field. Capacity via `[observability].cdr_ring_size` (default 50; `0` ⇒ empty list). In-memory only: a tail for sightglass's history pane and incident curls, not CDR retention — configure `[cdr]` sinks for that. |

### Deprecated unversioned aliases

The five routes that predate the `/admin/v1/` namespace (0.6.0) are still
served at their original unversioned paths — same handlers, same roles, same
responses. Existing scripts keep working; new tooling should use the v1
forms. The aliases will not be removed before a 1.0.

| Deprecated alias | Canonical route | Difference |
|------------------|-----------------|------------|
| `GET /admin/calls` | `GET /admin/v1/calls` | none |
| `POST /admin/calls/:id/hangup` | `POST /admin/v1/calls/:id/hangup` | **id namespace**: the alias takes the **SIP** Call-ID (`sip_call_id` from the listing); the v1 route takes the **bridge** `call_id`. Neither accepts the other's id (a wrong-namespace id is a `404`) — the listing returns both, so use whichever endpoint matches the id you hold. |
| `GET /admin/registrations` | `GET /admin/v1/registrations` | none |
| `GET`/`PUT /admin/log` | `GET`/`PUT /admin/v1/log` | none |
| `POST /admin/hep/test` | `POST /admin/v1/hep/test` | none |

The `siphon_ai_admin_requests_total{endpoint=…}` metric labels the alias and
the v1 form separately, so a dashboard can watch legacy-path traffic drain to
zero before anyone considers retiring the aliases.

### Admin auth & RBAC

Define the listener and at least one token under `[admin]` (full field
reference in `docs/CONFIG.md` `[admin]`):

```toml
[admin]
# Dedicated listener for /admin/*. Bind to loopback (or a private
# interface). On a routable bind, set [admin.tls] below so the bearer
# token is encrypted on the wire (otherwise it's plain HTTP).
listen = "127.0.0.1:9092"

# One block per token. The secret is hashed (SHA-256) at load and
# compared in constant time; it is never logged. role ∈ readonly |
# operator | admin (roles nest: readonly ⊂ operator ⊂ admin).
[[admin.token]]
name  = "dashboard"          # actor label in audit logs (not a secret)
token = "${SIPHON_ADMIN_RO}" # ${VAR} expansion works — keep secrets out of the file
role  = "readonly"

[[admin.token]]
name  = "ops-oncall"
token = "${SIPHON_ADMIN_OP}"
role  = "operator"

[[admin.token]]
name  = "automation"
token = "${SIPHON_ADMIN_ADMIN}"
role  = "admin"

# Optional (0.18.0): serve /admin/* over HTTPS so the bearer token is
# encrypted on a routable bind — no TLS-terminating proxy needed. Both
# cert and key are required; the cert hot-reloads on SIGHUP (same as
# [sip.tls]). Secret paths can use ${file:…}/${cred:…} too.
[admin.tls]
cert = "/etc/siphon-ai/admin.crt"
key  = "/etc/siphon-ai/admin.key"
```

Roles, lowest to highest (each inherits everything below it):

| Role       | Can do |
|------------|--------|
| `readonly` | All `GET` / list endpoints (calls, registrations, log, conferences, parked) — **except `GET /admin/v1/calls/:id/sip`**, see below. |
| `operator` | Everything `readonly` can, plus hangup, park / retrieve, conference create / end / add / remove, and **reading raw SIP** (`GET /admin/v1/calls/:id/sip`). |
| `admin`    | Everything `operator` can, plus **billable** origination (`POST /admin/v1/calls`), `PUT /admin/v1/log`, and `POST /admin/v1/hep/test`. |

> **An `operator` token can read recent SIP credentials, on a default
> install.** `GET /admin/v1/calls/:id/sip` returns messages verbatim,
> including `Authorization` and `Proxy-Authorization` digest headers,
> and the SIP ladder ring is **on by default**
> (`[observability].sip_ring_size = 50`). This is deliberate
> (DESIGN_SIP_LADDER.md §2): a redacted ladder invites the wrong
> conclusion about what actually went over the wire, so the access
> boundary is the role rather than the content — which is also why this
> is the one `GET` on the API gated above `readonly`.
>
> Two consequences worth planning for:
>
> - **Treat an `operator` token as credential-bearing.** It was already
>   powerful (it can end live calls); it is now also readable-secrets
>   powerful. Hand out `readonly` for dashboards and NOC wall screens —
>   sightglass's `--read-only` flag and a `readonly` token both work.
> - **Set `[observability].sip_ring_size = 0` to opt out.** Capture stops,
>   nothing is held, and the endpoint answers `501`. This is a supported
>   configuration, not a vestigial switch; clients degrade to "disabled"
>   rather than treating the node as down.
>
> The ring holds no class of data the node was not already handling —
> these bytes are in the daemon's address space as the parsed dialog
> anyway, and as the HEP packet shipped to Homer wherever `[hep]` is on.
> What defaulting it on changes is their *lifetime* (minutes, bounded by
> `sip_ring_size` / `sip_ring_max_messages`) and their *reachability*
> (an authenticated operator endpoint). Nothing is written to disk.

Every request is audited: a structured log line (actor = token **name**,
role, endpoint template, result, peer address — never the secret) and the
`siphon_ai_admin_requests_total{endpoint, role, result}` counter
(`result` ∈ `ok` | `unauthenticated` | `forbidden` | `not_found` | `error`).
`result` reflects the **response status**, not just the auth outcome: a
matched, authorized handler that returns `404` (a stale call/room id) counts
`not_found`, and any other handler failure (400 / 409 / 429 / 501 / 503)
counts `error` — so `result != "ok"` is a faithful failure signal.

A bearer token goes on every call:

```sh
ADMIN=http://127.0.0.1:9092          # https://… when [admin.tls] is set
curl -s -H "Authorization: Bearer $SIPHON_ADMIN_RO" $ADMIN/admin/v1/calls
# missing/invalid token → 401 + WWW-Authenticate: Bearer
# token below the endpoint's min role → 403
```

> **Upgrade note (0.10.0, breaking).** Before 0.10.0 `/admin/*` was
> unauthenticated on the `[observability].http_listen` port. It has moved
> to the dedicated `[admin]` listener and now requires a token. **Action
> required:** add an `[admin]` block with at least one token, point admin
> tooling at the new port with an `Authorization: Bearer …` header, and
> drop any `/admin/*` allow rules from the metrics port. If you omit
> `[admin]` entirely, `/admin/*` is **not served at all** (secure default)
> — the daemon still starts and serves metrics/health. A reverse proxy
> that previously added auth in front of the metrics port should move that
> auth (or hand off to the native tokens) to the admin port.

### `POST /admin/v1/calls` — outbound origination

Requires `[outbound].max_concurrent > 0` and at least one `[[gateway]]` (see
`docs/CONFIG.md`; full guide: `docs/OUTBOUND.md`). **This endpoint places
billable calls** and requires an **`admin`-role** token (see
[Admin auth & RBAC](#admin-auth--rbac)). The `max_concurrent` cap +
`rate_limit_per_sec` are the native guardrails on top of the role gate.

```sh
curl -X POST -H "Authorization: Bearer $SIPHON_ADMIN_ADMIN" \
  http://127.0.0.1:9092/admin/v1/calls -d '{
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

### `/admin/v1/conferences` — conference admin (0.7.0)

Requires `[conference].enabled = true` (all routes `501` otherwise). The
list route needs `readonly`; create / end / add / remove need `operator`
(see [Admin auth & RBAC](#admin-auth--rbac)). A room is N calls sharing one
mixed audio room; see `docs/CONFIG.md` `[conference]` and the WS protocol's
`conference_*` messages (`docs/PROTOCOL.md` §3.12 / §4.8).

```sh
ADMIN=http://127.0.0.1:9092
# Who's in which room (readonly)
curl -s -H "Authorization: Bearer $SIPHON_ADMIN_RO" $ADMIN/admin/v1/conferences
# → {"count":1,"conferences":[{"room_id":"support-7","sample_rate":8000,
#     "participants":["siphon-a","siphon-b"]}]}

# Pull an active call into a room (creates it if absent) — operator
curl -X POST -H "Authorization: Bearer $SIPHON_ADMIN_OP" \
    $ADMIN/admin/v1/conferences/support-7/participants \
    -d '{"call_id":"siphon-c"}'        # → 202

# Drop one call back to its private bot — operator
curl -X DELETE -H "Authorization: Bearer $SIPHON_ADMIN_OP" \
    $ADMIN/admin/v1/conferences/support-7/participants/siphon-c  # → 202

# End the whole room (every member reverts to its direct pair) — operator
curl -X DELETE -H "Authorization: Bearer $SIPHON_ADMIN_OP" \
    $ADMIN/admin/v1/conferences/support-7   # → 200
```

`add`/`remove` participant return **`202` (dispatched)**: the daemon signals
the target call, which joins/leaves on its own WS session — the actual
outcome surfaces there as `conference_joined` / `conference_left` / `error`,
not in this HTTP response. `add` is `404` only when no active call has that
bridge `call_id`. `create` returns `201 {"room_id"}` (a generated id when the
body omits `room_id`); `409` if the id is already live; `503` at the
`max_rooms` cap; `400` for a `sample_rate` other than 8000/16000.

Example: bump bridge logging to debug for an incident, then revert.

```sh
ADMIN=http://127.0.0.1:9092
# GET is readonly; PUT (mutates the running filter) is admin.
prev=$(curl -s -H "Authorization: Bearer $SIPHON_ADMIN_RO" $ADMIN/admin/v1/log)
curl -X PUT -H "Authorization: Bearer $SIPHON_ADMIN_ADMIN" \
    --data 'siphon_ai=info,siphon_ai_bridge=debug' $ADMIN/admin/v1/log
# … reproduce the issue …
curl -X PUT -H "Authorization: Bearer $SIPHON_ADMIN_ADMIN" \
    --data "$prev" $ADMIN/admin/v1/log
```

### `/admin/v1/parked` — park admin (0.7.0)

Requires `[park].enabled = true` (all routes `501` otherwise). The list
route needs `readonly`; park / retrieve need `operator` (see
[Admin auth & RBAC](#admin-auth--rbac)). Park shelves a call playing hold
music with **no** WS session; see `docs/CONFIG.md` `[park]` and the WS
protocol (`docs/PROTOCOL.md` §4.9).

```sh
ADMIN=http://127.0.0.1:9092
# What's parked, and for how long (readonly)
curl -s -H "Authorization: Bearer $SIPHON_ADMIN_RO" $ADMIN/admin/v1/parked
# → {"count":1,"parked":[{"call_id":"siphon-a","slot":"lot-3","parked_secs":42}]}

# Park an active call (slot label optional) — operator
curl -X POST -H "Authorization: Bearer $SIPHON_ADMIN_OP" \
    $ADMIN/admin/v1/calls/siphon-a/park \
    -d '{"slot":"lot-3"}'        # → 202

# Retrieve it onto a fresh WS session (ws_url optional — defaults to the
# call's original bridge ws_url) — operator
curl -X POST -H "Authorization: Bearer $SIPHON_ADMIN_OP" \
    $ADMIN/admin/v1/calls/siphon-a/retrieve \
    -d '{"ws_url":"wss://my-bot.example/retrieve"}'   # → 202
```

`park`/`retrieve` return **`202` (dispatched)**: the daemon signals the
target call and its own controller does the work — the outcome surfaces on
the call's (old/new) WS session and the `call_parked` / `call_retrieved`
webhooks, not in this HTTP response. A park refused by the `[park].max_parked`
cap is **not** a `503` here — the cap is enforced in the call's controller and
surfaces as `error { code: "park_failed" }` on its WS while the call continues
unparked. `retrieve` is `409` when the named call exists but isn't parked.

## CDR consumers

When `[cdr.file]` is enabled, the daemon appends one record per ended
call to the configured path — a JSON object per line by default, or a
fixed-column CSV row with `[cdr.file].format = "csv"` (0.36.0; see below).
Rotate the file with `logrotate`; the daemon re-opens on `SIGHUP` (in
practice — restart is simpler).

```json
{
  "version": 9,
  "call_id": "siphon-6ce27797cc0a4997b90cbae2f46ce7a4",
  "sip_call_id": "1-2651348@127.0.0.1",
  "started_at":  "2026-05-12T18:10:32.481Z",
  "answered_at": "2026-05-12T18:10:32.512Z",
  "ended_at":    "2026-05-12T18:11:04.117Z",
  "duration_ms": 31636,
  "from": "sipp",
  "to":   "1000",
  "direction": "inbound",
  "leg_transport": "udp",
  "media_type": "rtp",
  "route": "default",
  "ws_url": "ws://echo-ws:8765/",
  "audio":   { "codec": "PCMU", "payload_type": 0, "sample_rate": 8000 },
  "termination": {
    "cause": "caller_hangup",
    "bridge_disconnect": "stop_sent",
    "tap_disconnect":    "call_ended"
  },
  "quality": {
    "first_audio_out_ms": 742,
    "barge_in_count": 3,
    "avg_jitter_ms": 11.5,
    "max_jitter_ms": 30.0,
    "avg_packet_loss_ratio": 0.004,
    "max_packet_loss_ratio": 0.02,
    "avg_rtcp_rtt_ms": 41.7,
    "rx_packets_received": 14820,
    "rx_packets_lost": 12,
    "rx_packets_out_of_order": 3,
    "rx_packets_duplicate": 0,
    "tx_packets_sent": 14900,
    "tx_octets_sent": 2384000,
    "tx_packets_lost_reported": 12,
    "mos_estimate_min": 3.9,
    "mos_estimate_avg": 4.3
  }
}
```

`termination.cause` values for a call that went active: `"caller_hangup"`
(the far end sent BYE — by far the most common ending; 0.40.0, CDR
`version` 5), `"server_hangup"`, `"local_shutdown"` (admin force-hangup,
CANCEL, or RFC 4028 session expiry — before 0.40.0 this also absorbed
remote hangups), `"drain_forced"` (force-ended at the graceful-shutdown
drain deadline, 0.17.0 — CDR `version` 3), `"bridge_ended"` (an orderly
WS ending on SiphonAI's side that isn't one of the richer causes),
`"ws_disconnect"` (the WS connection dropped unexpectedly mid-call —
server crash, network cut, keepalive timeout, or a bare close with no
`stop` exchange; also a reconnect window that elapsed without
recovering — 0.45.0, CDR `version` 7; before that these collapsed into
`"bridge_ended"`), `"tap_ended"`, `"transfer"` (the server transferred
the call away and the peer accepted the REFER — 0.41.x, CDR `version` 6;
matches the WS `stop` reason, which had reported it correctly all along).
`tap_disconnect` adds `"inactivity_timeout"` when the RTP watchdog fired.

`duration_ms` is `ended_at - started_at` — **wall-clock including ring /
setup time**, not connected time. For **outbound** calls `started_at` is
stamped when the origination request is accepted, before the INVITE goes
out, so it precedes the answer by however long the call rang; for inbound
it coincides with the answer. Billable duration is
`ended_at - answered_at` (0.40.0, CDR `version` 5). `answered_at` is
absent when the call never connected, which is also what distinguishes an
unanswered call from a very short one.

A **delayed-offer** call that fails negotiation before going active
(0.9.5) also gets a CDR, with one of: `"ack_timeout"` (no ACK before SIP
Timer H), `"missing_sdp_answer"`, `"invalid_sdp_answer"`,
`"no_compatible_codec"`, or `"invalid_remote_media"`. These records have
an **empty `audio`** block (no codec was negotiated) and blank
`bridge_disconnect` / `tap_disconnect`.

The `version` integer is **9** as of 0.51.0 (the `leg_transport` and
`media_type` fields and their two CSV columns; 8 in 0.48.8 for the
`recording_result` field and its CSV column; 7 in 0.45.0 for the `ws_disconnect` cause, 6 in
0.41.x for `transfer`, 5 in 0.40.0 for `answered_at` + `caller_hangup`, 4
in 0.30.0 for the optional `quality` block, 3 in 0.17.0 for
`drain_forced`, 2 in 0.9.5 for the delayed-offer causes). It bumps on
changes that could break a strict consumer. Adding a new optional *field*
to the JSON shape is additive on its own — v4 and v8 bumped anyway, the
first so consumers could gate on the version rather than probe for the
`quality` block, the second because the field also widens the CSV row.

### `leg_transport` and `media_type` (v9)

Every record at `version >= 9` carries both:

- **`leg_transport`** — `udp` | `tcp` | `tls` | `ws` | `wss`. For an
  inbound call, the transport the INVITE arrived on; for an outbound
  one, the gateway's configured transport.
- **`media_type`** — `rtp` (cleartext), `srtp` (a classic leg keyed by
  SDES or DTLS through SDP), or `webrtc` (a browser leg: ICE, BUNDLE,
  `a=rtcp-mux`, DTLS-SRTP — encrypted by construction).

They answer different questions and should not be conflated: `wss`
signalling says nothing about whether the audio was encrypted, and
`udp` signalling with SDES media is a real deployment. **`media_type`
is the compliance field** — `rtp` is the only value for which the audio
crossed the network in the clear.

`media_type` reports what actually happened, not what was configured: a
`[[gateway]].srtp = "preferred"` trunk whose peer answered plaintext
records `rtp`, because that is what was on the wire. `webrtc` is its own
value rather than `srtp` with a flag because the negotiation, the
failure modes and the metrics all differ — see
`siphon_ai_webrtc_legs_total` above.

Both are omitted (rather than guessed) in the two cases where they are
genuinely unknown: a record written by a pre-0.51.0 daemon, and a
delayed-offer call that failed negotiation before any media existed —
that record has `leg_transport` but no `media_type`, since claiming
`rtp` would say cleartext audio flowed when none did. Gate on
`version >= 9` rather than probing.

### CSV format (`[cdr.file].format = "csv"`, 0.36.0)

The CSV layout is a flat view of the same record — 52 columns, one row
per call, RFC 4180 quoting. (The count last read 45 before 0.38.0's three
`quality_tx_*` columns, 0.40.0's `answered_at`, 0.48.8's
`recording_result`, and 0.51.0's `leg_transport` + `media_type`; it is
asserted against the header by a unit test, so trust the code if this
drifts again.) A header row is written when the file starts
empty (never repeated on restart). Semantics:

- Nested blocks flatten to prefixed columns: `audio_codec`,
  `termination_cause`, `consent_announced`, `park_count`, `hold_total_ms`,
  `reconnect_count`, `quality_avg_jitter_ms`, `quality_mos_estimate_avg`, …
- An absent block or unmeasured value is an **empty cell**, not a zero —
  the same "clean vs unmeasured" distinction the JSON shape makes.
- Enum values (`direction`, `termination_cause`) use the same snake_case
  wire strings as JSON; timestamps are RFC 3339 UTC with milliseconds.
- New columns are only ever **appended**, so position-keyed ingestors
  survive additive schema changes. Prefer keying by header name anyway.
- Switching an existing file's format mixes layouts in one file — point a
  format change at a new `path`. The webhook sink is unaffected (always
  JSON).

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

Optional recording fields appear when the call was subject to recording
(`recording_id`/`recording_path` added in 0.5.0; `recording_result` bumps
the schema to v8 — all omitted when recording is off). They are **not**
co-present: a `blocked` call carries `recording_result` (and the `consent`
block) but no `recording_id`/`recording_path`, since no file was ever
created — gate on `version >= 8`, then check `recording_result`, before
touching the path:

- `recording_id` — identifies the recording (equals `call_id` in this
  release).
- `recording_path` — filesystem path of the WAV. Present even when the
  recording `failed` (it points at where the file would be), so it is
  **not** on its own a promise that a playable file exists there.
- `recording_result` — `ok` / `degraded` / `failed` / `blocked` (0.48.8,
  issue #441), the same vocabulary as the `siphon_ai_recordings_total`
  metric. This is what makes `recording_path` interpretable: previously
  the outcome existed only in the process-wide metric, which cannot be
  attributed to a call, so a record naming a file that was never written
  was indistinguishable from one naming a good recording. `blocked` means
  a configured `[recording.announcement]` did not play **to completion**
  for a call that was actually going to record — `mode = "always"`, or an
  on-demand call whose server sent `start_recording` (issue #446) — so
  capture never started and no file exists. That covers both an unusable
  prompt (issue #440) and a prompt cut short by a hold or park (issue
  #445 — a partially heard prompt is not consent; the fail-close is
  deliberate). An on-demand call with an incomplete prompt that the
  server never asked to record stamps **no** `recording_result` (there is
  no recording outcome to report), just the `consent` block.

  A call that ends — or loses its WS session — while the consent
  announcement is still playing also stamps no `recording_result`
  (issue #444): the `consent` block alone (`announced: false`) tells
  that story. So the reconciliation reading is: `consent` present +
  `recording_result` absent = the call ended before consent completed;
  `recording_result: "blocked"` = the call was going to record and the
  consent prompt failed or was interrupted.

One optional park object appears when the call was parked at least once
(added in 0.7.0; schema stays at version 1 — omitted when the call was
never parked):

- `park` — `{ "count": <episodes>, "total_ms": <cumulative parked ms> }`.
  A call can park/retrieve repeatedly, so `count` is the number of park
  episodes and `total_ms` is the summed parked wall-time across them.

One optional hold object appears when the bot held its own caller at least
once (added in 0.7.2; schema stays at version 1 — omitted when the call was
never bot-held):

- `hold` — `{ "count": <episodes>, "total_ms": <cumulative held ms> }`.
  Same shape as `park`. Counts only **bot-initiated** holds (the WS server
  sent `hold`; see PROTOCOL.md §4.10) — a far-end hold is the peer's
  business and is not tallied here.

One optional reconnect object appears when the WS dropped and reconnect ran
at least once (added in 0.7.3; schema stays at version 1 — omitted when the
call never reconnected):

- `reconnect` — `{ "count": <episodes>, "total_gap_ms": <cumulative ms on reconnect hold music> }`.
  An episode is one unexpected WS drop that entered the reconnect path
  (`[bridge].ws_reconnect_enabled`; see PROTOCOL.md §5.7). Cross-check
  `siphon_ai_ws_reconnects_total` for the recovered/exhausted split.

One optional quality object appears when the call produced any quality
signal (added in 0.30.0 — **CDR `version` 4**; omitted for calls that
never went active). Fields inside are individually optional — a signal
that never produced data is omitted, not zeroed, so `"clean"` and
`"unmeasured"` stay distinguishable:

- `quality.first_audio_out_ms` — ms from "WS `start` on the wire" to the
  first server audio frame reaching playout toward the caller: the
  end-to-end first-token latency of the operator's STT/LLM/TTS chain.
  Pair with `siphon_ai_ws_connect_seconds` to decompose connect time.
- `quality.barge_in_count` — playout clears over the call: `auto_clear`
  firings (daemon-side barge-in) plus server-sent `clear` commands.
- `quality.avg/max_jitter_ms`, `avg/max_packet_loss_ratio`,
  `avg_rtcp_rtt_ms` — aggregates over the call's RTCP Receiver Reports
  (remote-reported: how the far end received SiphonAI's stream).
  **`avg/max_packet_loss_ratio` are means/maxima of per-interval
  fractions**, each sample being one RR's `fraction_lost` (loss since the
  previous report, RFC 3550 §6.4.1) — *not* the call's cumulative loss
  ratio, and they will not reconcile against a carrier's cumulative
  figure. Through 0.37.x these were documented as cumulative; the values
  never changed, only the description was wrong. For a whole-call loss
  rate use `tx_packets_lost_reported / tx_packets_sent`.
- `quality.rx_packets_*` — end-of-call totals measured locally on the
  caller→SiphonAI stream (received / lost / out-of-order / duplicate).
- `quality.tx_packets_sent` / `tx_octets_sent` (0.38.0) — end-of-call
  totals measured locally on the SiphonAI→caller stream. `tx_octets_sent`
  counts RTP **payload** octets only (no headers, no SRTP overhead), the
  same basis as an RTCP SR's sender octet count.
- `quality.tx_packets_lost_reported` (0.38.0) — the far end's own
  absolute count of packets it lost on that stream, from the call's last
  RR. With `tx_packets_sent` this gives operators the sentence they
  actually ask for after a bad call: *"we sent 14,900 packets; the far
  end reported 12 lost."* **Signed** — a negative value is legitimate
  (duplicates pushing the peer's received count past expected), so don't
  clamp it.
- `quality.mos_estimate_min/avg` — worst / mean transport-only MOS-CQE
  estimate (1.0–5.0; see PROTOCOL.md §3.8 `mos_estimate`). RX-only by
  construction: it scores what SiphonAI heard, so the `tx_*` counters
  don't feed it.

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
`[cdr.webhook].retry_max` times with exponential backoff, then drops —
unless `[cdr.webhook].spool_dir` is set, which makes delivery durable
across restarts. Set `[cdr.webhook].secret` to sign each POST. See
[Webhook delivery: signing, idempotency, durability](#webhook-delivery-signing-idempotency-durability).

## Quality record consumers

With `[quality]` enabled (0.31.0), the daemon emits per-call quality
history records: one per call per `interval_secs` **plus a final
end-of-call summary**. A record is the CDR `quality` block flattened to
the top level, with framing fields:

```json
{
  "version": 1,
  "kind": "interval",
  "call_id": "siphon-6ce27797cc0a4997b90cbae2f46ce7a4",
  "ts": "2026-07-13T21:14:07.812Z",
  "seq": 3,
  "barge_in_count": 1,
  "first_audio_out_ms": 742,
  "avg_jitter_ms": 11.5,
  "max_jitter_ms": 30.0,
  "avg_packet_loss_ratio": 0.004,
  "max_packet_loss_ratio": 0.02,
  "rx_packets_received": 4820,
  "rx_packets_lost": 4,
  "rx_packets_out_of_order": 1,
  "rx_packets_duplicate": 0,
  "tx_packets_sent": 4850,
  "tx_octets_sent": 776000,
  "tx_packets_lost_reported": 2,
  "mos_estimate_min": 4.1,
  "mos_estimate_avg": 4.3
}
```

- `kind` — `"interval"` (cadence sample) or `"final"` (end-of-call; the
  same numbers the CDR carries).
- `seq` — per-call record counter from 0; the final record continues
  the sequence.
- Counters are **cumulative since call start** — diff successive `seq`s
  for rates. Unmeasured fields are omitted, not zeroed. Records with
  nothing measured at all are skipped.
- File sink: append-only JSONL, same rotation story as the CDR file.
  Webhook sink: HMAC-signed + spooled exactly like `[cdr.webhook]`
  (label `sink="quality"` on the delivery metrics).

For a live *right-now* probe of one call, use
`GET /admin/v1/calls/{id}/stats` (readonly role) instead of waiting for
the next record. See `docs/OPERATIONS.md` for the end-to-end ingestion
pipeline into Loki/Grafana.

## Lifecycle webhooks

Off-band events (NOT the per-call WS bridge). Same delivery transport as the
CDR webhook — see [Webhook delivery: signing, idempotency,
durability](#webhook-delivery-signing-idempotency-durability) for signing,
the idempotency id, and the durable spool. Event types:

| `type`                          | When                                             |
|---------------------------------|--------------------------------------------------|
| `call_start`                    | After 200 OK has gone out on an accepted INVITE. |
| `call_end`                      | After the controller exits and the CDR record is built (inbound *and* answered outbound calls). |
| `registration_state_changed`    | Each `[[register]]` state transition (`pending` → `registered`, `registered` → `failed`, etc.). |
| `outbound_initiated`            | An originated call (`POST /admin/v1/calls`) was admitted and its INVITE is going out. |
| `outbound_answered`             | The callee answered (2xx) and the WS bridge is starting. |
| `outbound_failed`               | The originated call ended without an answer. Terminal — no `call_end`/CDR follows. |
| `conference_created`            | A conference room was created — first `conference_join` for a `room_id`, or an admin pre-create (0.7.0). Carries `room_id`, `sample_rate`. |
| `conference_ended`              | A conference room ended — last member left, or an operator force-ended it (0.7.0). Carries `room_id`, `duration_ms`, `peak_participants`. Pairs 1:1 with `conference_created`. |
| `call_parked`                   | A call was parked — WS `park` or `POST /admin/v1/calls/:id/park` (0.7.0). Carries `call_id` and the optional `slot` label. |
| `call_retrieved`                | A parked call was retrieved onto a fresh WS session — `POST /admin/v1/calls/:id/retrieve` (0.7.0). Carries `call_id` and the `ws_url` the new session connected to. |
| `park_timeout`                  | A parked call hit `[park].timeout_secs` (0.7.0). Carries `call_id` and `action` (`"hangup"` or `"keep"`, per `[park].timeout_action`). |
| `recording_uploaded`            | A call's recording finished uploading to object storage (`[recording.storage]`, 0.25.0). Arrives *after* the call's `call_end` (upload is asynchronous). Carries `call_id`, `recording_id`, `url` (`s3://bucket/key`, matching the CDR's `recording_url`), and `size_bytes`. |

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

A parked call emits `call_parked`, then (when retrieved) `call_retrieved`,
or (on timeout) `park_timeout`. A call may park/retrieve repeatedly.

```json
{ "type": "call_parked", "version": 1, "call_id": "siphon-…",
  "timestamp": "2026-06-14T10:00:00Z", "slot": "lot-3" }
{ "type": "call_retrieved", "version": 1, "call_id": "siphon-…",
  "timestamp": "2026-06-14T10:02:30Z", "ws_url": "wss://my-bot.example/retrieve" }
{ "type": "park_timeout", "version": 1, "call_id": "siphon-…",
  "timestamp": "2026-06-14T10:05:00Z", "action": "hangup" }
```

## Webhook delivery: signing, idempotency, durability

Lifecycle webhooks (`[webhooks]`) and the CDR webhook (`[cdr.webhook]`)
share one delivery transport, so the following applies identically to both
(0.11.0). All of it is **additive** — the JSON bodies are unchanged
(webhook + CDR schema versions are **not** bumped); these are
transport-layer headers and behavior.

### Headers on every delivery

| Header | When | Purpose |
|--------|------|---------|
| `X-SiphonAI-Event-Id` | always | A UUIDv4 unique to one logical delivery, **stable across retries and any spool replay**. |
| `Idempotency-Key` | always | Alias of `X-SiphonAI-Event-Id` for receivers/middleware that key on the conventional name. |
| `X-SiphonAI-Signature` | when `secret` set | `t=<unix>,v1=<hex>` — HMAC-SHA256 over `"<unix>.<raw-body>"`. |

**Idempotency.** Delivery is *at-least-once* (a retry or a post-restart
spool replay can redeliver a body the receiver already processed but failed
to ACK). Dedupe on `X-SiphonAI-Event-Id` — persist seen ids and skip
duplicates.

**Signature verification.** Set `secret` to enable
`X-SiphonAI-Signature`. The timestamp is inside the signed string, so a
captured POST can't be replayed outside your freshness window. To verify:
split `t=`/`v1=`, recompute the HMAC over `"<t>.<raw-request-body>"` with
your secret, compare in constant time, and reject if `t` is too old.

```python
# Flask receiver — verify a SiphonAI webhook/CDR POST.
import hmac, hashlib, time
from flask import request, abort

SECRET = b"whsec_..."          # same value as [webhooks].secret
MAX_SKEW = 300                 # seconds

def verify(req):
    sig = req.headers.get("X-SiphonAI-Signature", "")
    parts = dict(p.split("=", 1) for p in sig.split(",") if "=" in p)
    t, v1 = parts.get("t"), parts.get("v1")
    if not t or not v1 or abs(time.time() - int(t)) > MAX_SKEW:
        abort(401)                                   # missing / stale
    signed = f"{t}.".encode() + req.get_data()       # raw body bytes
    expected = hmac.new(SECRET, signed, hashlib.sha256).hexdigest()
    if not hmac.compare_digest(expected, v1):
        abort(401)                                   # bad signature
    return req.headers["X-SiphonAI-Event-Id"]        # dedupe on this
```

> Verify against the **raw request body bytes**, before any JSON
> re-serialization — SiphonAI signs the exact bytes it sends.

### Durability (spool)

Without `spool_dir`, delivery is best-effort: after `retry_max` in-memory
retries (exponential backoff) a failed delivery is logged and dropped.

Set `spool_dir` to make delivery durable. A delivery that exhausts the
in-memory budget is written to that directory and re-attempted by a
background worker; the worker resumes pending entries after a daemon
**restart**. The happy path is unchanged (no disk I/O — entries are only
written on failure). Entries are retried oldest-first with capped backoff (10 s
doubling to a 5-minute ceiling). A `4xx` rejection is discarded on the
first attempt — that is the receiver telling you the payload is bad.

**Durability has a horizon.** An entry that keeps failing is discarded
once it is older than `spool_max_age_secs` (default 72 h; `0` disables
the age check entirely). That value is your tolerance for a *receiver
outage*: if the receiver is down longer, the events that age out while
it is away are lost, counted as
`siphon_ai_webhook_deliveries_total{result="dropped"}` and logged with
`spooled delivery exceeded spool_max_age_secs`. Set `0` for a stream
that must not lose records — an audit sink in particular — and rely on
the file cap instead.

Before 0.48.12 the horizon was a hard-coded 100 drain attempts, which
the backoff turned into exactly 8 hours, undocumented and
unconfigurable (#467). The directory is created and write-probed at
startup, so a bad path fails the daemon loudly rather than at the first
failed delivery. Bound disk with the per-sink file cap (a full spool drops
the newest delivery rather than evicting an already-persisted one).

Point the file sink (`[cdr.file]`) at the same record stream if you want a
second, append-only durable copy of CDRs independent of HTTP delivery.

### Delivery health

Watch `siphon_ai_webhook_deliveries_total{sink,result}` (terminal
outcomes), `siphon_ai_webhook_delivery_attempts_total{sink,outcome}` (per
HTTP attempt), `siphon_ai_webhook_spool_depth{sink}` (a rising value means
deliveries are failing and backing up on disk), and
`siphon_ai_webhook_delivery_seconds{sink}` (latency incl. spool dwell). See
the [Metrics](#metrics) table. Per-delivery detail lives in the logs,
keyed by `event_id` (the audit-friendly id, not the secret).

## HEP / Homer

See `docs/HEP.md` for the architecture and `examples/homer-stack/` for a
local Homer + heplify-server + Postgres compose stack.

Quick check that emission is live:

```sh
curl -s http://localhost:9091/metrics | grep siphon_ai_hep
```

`siphon_ai_hep_packets_sent_total` should be incrementing across calls, and
`siphon_ai_hep_packets_dropped_total{reason="queue_full"}` should stay flat.
Both series exist from startup, before the first call.

Note that **neither proves the collector is receiving**: `sent` counts
wire-level success, so a black-holing NAT still counts. A collector that is
down shows up as a throttled warning instead —

```sh
journalctl -u siphon-ai | grep hep_rs
# WARN hep_rs::udp: HEP UDP send failed ... error=Connection refused
```

— which is why there is no `siphon_ai_hep_collector_up` gauge. `docs/HEP.md`
has the full diagnostic order.

## Metrics

All histograms have sensible default buckets defined explicitly — no reliance
on the metrics crate's defaults (CLAUDE.md §7.4) — and every metric below
carries a `# HELP` line on the endpoint. Both are enforced by tests over the
full metric list in `crates/telemetry/src/metrics.rs`, so a metric can't ship
undescribed or unbucketed (issue #431, which found eleven of the former and
four of the latter). This covers the embedded forge crates' histograms too:
bucket registration is exporter-side, so forge-engine's suggested buckets are
applied (and test-enforced) in our `prometheus_builder()` (issue #437).

> **Dashboards & alerts as code (0.21.0).** You don't have to author these
> from scratch: [`examples/observability/`](../examples/observability/) ships
> a runnable Prometheus + Grafana stack — recording rules, starting-point
> alerting rules, and two Grafana dashboards (Fleet Overview + Call Quality)
> built against the metrics below. `docker compose -f
> examples/observability/compose.yaml up`. A CI check keeps the metric names
> in those artifacts in sync with this table.

| Metric                                  | Type      | Labels                                | What it measures |
|-----------------------------------------|-----------|---------------------------------------|------------------|
| `siphon_ai_invites_total`               | counter   | `result=accepted\|rejected\|rejected_attestation\|rejected_capacity\|rejected_trunk\|rejected_webrtc\|no_match` | INVITEs by acceptance outcome. `rejected_attestation` is a STIR/SHAKEN policy reject (`min_attestation` gate or `require_identity`) — separately alertable from ordinary routing/media `rejected`. **`rejected_trunk`** (#564) is the `[[trunk]]` allowlist gate's `403 Forbidden` — an INVITE from a source matching no configured trunk, i.e. exactly the scanner traffic the gate exists to shed. **Alert on it**: a sustained rate is either an attack on your SIP port or a misconfigured peer that should be in a trunk's `peer_addrs`. Per-peer detail is in the audit stream (`invite_rejected`, reason `no_trunk`) and the SIP ring — the metric deliberately carries no peer label (unbounded cardinality). **`rejected_webrtc`** is a browser call this daemon could not serve: an INVITE arrived over WS/WSS with a WebRTC-shaped offer while `[webrtc]` was disabled, or enabled on a build that cannot yet terminate the media leg. **Alert on it** — it means someone pointed a browser at this node and got nothing, which is a deployment gap rather than a bad call; the rejection's log line says which of the two it was. Digest-auth 401s are *not* in this counter — the same INVITE normally retries with credentials and is counted then; brute-force alerting for that gate is `siphon_ai_sip_auth_total{result="failed"}`. **`rejected_capacity`** (#554) is the RTP port pool being full: the call was refused `503 Service Unavailable` + `Retry-After` because `[media].rtp_port_range` had nothing left, not because anything was wrong with it. **Alert on it** — on a node that also originates, the pool is shared and an outbound surge can exhaust it, with this counter the only inbound-side signal. It also covers a `[media].reserved_outbound_calls` refusal, where free ports remain but are held for origination; `siphon_ai_rtp_reserve_blocks_total` (below) separates the two. It is a sizing problem, not a fault; see `test-harness/load/RESULTS-0.49.9-mixed-and-soak.md`. |
| `siphon_ai_rtp_reserve_blocks_total`    | counter   | none                                  | Inbound INVITEs refused because the RTP port pool had reached `[media].reserved_outbound_calls` — ports were still free, but held for origination (#556). The refusal is indistinguishable from a genuinely exhausted pool on the wire and in `siphon_ai_invites_total` (both `503` + `Retry-After`, both `rejected_capacity`); this is the operator-side split. Non-zero means the reservation is actively shedding inbound load — the knob working, not a fault — but a sustained rate means `rtp_port_range` is too small for the traffic you are carrying. **Published as a zero baseline**, so "never shed" reads as `0` rather than as a missing series. |
| `siphon_ai_rtp_reserved_outbound_calls` | gauge     | none                                  | The configured value of `[media].reserved_outbound_calls`, in port pairs (= concurrent calls), published once at startup. `0` = unreserved pool. Sits next to the counter above so a dashboard can show the shed rate against the threshold that produced it without reading the TOML. |
| `siphon_ai_rtp_port_pairs_allocated`    | gauge     | none                                  | RTP port pairs currently allocated, **sampled from the pool itself** every 2 s (not incremented at call sites — a site-updated gauge under-counts under exactly the leak it exists to catch). Tracks `siphon_ai_calls_active`; **a value that stays above it — especially above zero with no calls — is a leaked media session**, the hung-dialog/port-leak class the teardown-soak harness phase guards (DEV_PLAN_WebRTC.md Phase 0). **Alert on divergence.** |
| `siphon_ai_rtp_port_pairs_capacity`     | gauge     | none                                  | Total RTP port pairs in the pool (`[media].rtp_port_range` / 2), republished by the same sampler. Static per process; with the gauge above it gives dashboards pool headroom without reading the TOML — the live complement of `rejected_capacity` above, which only fires once headroom is already gone. |
| `siphon_ai_registrar_registers_total`   | counter   | `result=ok\|challenged\|forbidden\|interval_too_brief\|rejected\|error` | REGISTER requests served by `[registrar]`. `challenged` is the normal digest first leg, not an attack signal. **Alert on `forbidden`**: an authenticated user's credentials trying to register an AOR they're not authorized for — someone claiming someone else's identity. |
| `siphon_ai_registrar_bindings`          | gauge     | none                                  | AORs currently registered with the daemon's registrar (`[registrar]`), republished on register/unregister and every sweeper pass. Falls when a stream-registered client's connection dies and the ~32 s grace elapses. Distinct from `siphon_ai_register_state`, which is this daemon registering as a *client* elsewhere. |
| `siphon_ai_calls_total`                 | counter   | `cause=caller_hangup\|server_hangup\|local_shutdown\|drain_forced\|bridge_ended\|ws_disconnect\|tap_ended\|transfer` | Ended calls by termination cause. `caller_hangup` (0.40.0) = the far end sent BYE, split out of `local_shutdown`, which now means admin force-hangup / CANCEL / session expiry only. `drain_forced` (0.17.0) = force-ended at the graceful-shutdown drain deadline. `transfer` (0.41.x) = the call was handed off via REFER. `ws_disconnect` (0.45.0) = the WS dropped unexpectedly mid-call (or reconnect never recovered), split out of `bridge_ended` — alert on this one; it's the WS server crashing or the network failing, not a call ending. Counts inbound **and** outbound legs (#373 — outbound legs were previously invisible here, so `ws_disconnect` alerting missed outbound WS crashes). |
| `siphon_ai_calls_active`                | gauge     | —                                     | Currently-running bridged calls, inbound and outbound (#373 — previously inbound-only). An outbound leg joins at answer; before that it counts only on `siphon_ai_outbound_calls_active`. |
| `siphon_ai_route_match_total`           | counter   | `route`                               | Calls per matched route. |
| `siphon_ai_tls_peer_identity_total`     | counter   | `result=verified\|none`               | Inbound INVITEs over TLS/WSS by whether the connection presented a client certificate the listener verified (mutual TLS, 0.51.0). `verified` = the INVITE carries a `PeerIdentity` and can match a `peer_cert_san` route; `none` = the peer presented nothing, which only `[sip.tls].client_auth = "optional"` (or no client auth) lets through. UDP/TCP INVITEs are not counted. Rolling mTLS out: run `optional`, drive `none` to zero, then switch to `required` — at which point `none` can only rise if the mode was reverted. Handshakes refused under `required` never reach this counter (the peer sends no INVITE); they are in sip-transport's handshake-error transport metrics. |
| `siphon_ai_verstat_total`               | counter   | `result=passed\|failed\|unsigned`     | STIR/SHAKEN verification outcomes per inbound INVITE. Emitted only when `[security.stir_shaken].enabled = true`. `passed` = every check held; `failed` = `Identity` header present but verification didn't fully pass; `unsigned` = no `Identity` header. |
| `siphon_ai_sip_auth_total`              | counter   | `result=ok\|challenged\|failed\|stale` | Inbound digest-auth outcomes per challenged INVITE (0.19.0). Emitted only for sources that require `[sip.auth]`. `ok` = a valid `Authorization` verified; `challenged` = no credentials presented → `401` issued; `failed` = credentials presented but wrong (bad password / unknown user) → `401`; `stale` = a nonce-freshness rejection → `401 stale=true` — the nonce was TTL-expired **or** past its reuse window (`[sip.auth].nonce_reuse_window_secs`, #430); the credential is not implicated either way. A rising `failed` is a brute-force / misconfiguration signal — pair with the fail2ban recipe. A rising `stale` usually just means peers re-authenticate less often than the reuse window — raise the window if the extra 401 round-trips bother you. |
| `siphon_ai_invite_admission_total`      | counter   | `result=accepted\|rate_limited\|dropped` | Inbound INVITE admission decisions (0.19.0). Emitted only when `[sip.admission]` is on. `accepted` = admitted; `rate_limited` = per-source rate trip or global `max_concurrent` cap → `503`; `dropped` = source flooding past `drop_after` → silently dropped (no response). A rising `dropped` means a sustained flood; `rate_limited` spikes with bursty peers or an undersized cap. |
| `siphon_ai_invite_admission_sources`    | gauge     | —                                     | Distinct source IPs currently tracked by per-source admission (0.19.0). Bounded by `[sip.admission].max_sources`. |
| `siphon_ai_recordings_total`            | counter   | `result=ok\|degraded\|failed\|blocked` | Recordings finished, when `[recording]` is on. `ok` = written cleanly; `degraded` = some 20 ms frames dropped under writer back-pressure (file is short, not corrupt); `failed` = an I/O error; `blocked` = a configured `[recording.announcement]` did not play to completion so capture never started, for a call that was actually going to record — `mode = "always"`, or on-demand with a server `start_recording` (0.48.8, issue #440; scoped in issue #446 so a broken prompt on on-demand calls nobody records doesn't tick at 100% of call volume; since issue #445 also covers a prompt cut short by a hold/park — fail-closed by design). Alert on `blocked` separately from `failed`: the first is consent playback (bad prompt, config push, or holds/parks landing mid-prompt), the second is disk. |
| `siphon_ai_outbound_calls_total`        | counter   | `result=answered\|busy\|declined\|no_answer\|rejected\|unreachable\|failed` | Outbound calls placed (0.6.0). `answered` = 2xx + bridged; `busy` = 486/600; `declined` = 403/603; `no_answer` = 408/480/487; `rejected` = other non-2xx; `unreachable` = DNS/transport/timeout with no response; `failed` = local media setup error. Classifies call **setup** only — an answered leg's eventual termination cause lands on `siphon_ai_calls_total{cause}` (#373). |
| `siphon_ai_transfers_total`             | counter   | `mode=blind\|attended`, `result=accepted\|rejected\|local_error` | REFER transfers attempted (0.6.1; also counts blind transfers, previously unmetered). `accepted` = 202 + call torn down; `rejected` = peer non-2xx; `local_error` = bad target / unknown consult call / dialog gone / send failure. |
| `siphon_ai_outbound_audio_frames_dropped_total` | counter | —                             | Outbound WS-server audio frames evicted (oldest-first) by the 200 ms playout window (PROTOCOL.md §5.5, #366). A nonzero rate means a WS server is streaming faster than realtime — an unpaced TTS integration (the server SDKs pace correctly) or a hostile peer. Pair with `mark` on the server side for burst-safe timing. |
| `siphon_ai_peer_hold_tx_suppressed_frames_total` | counter | —                            | Caller-leg 20 ms frames dropped because the negotiated direction forbade our send — we answered a peer hold with `recvonly`/`inactive` (RFC 3264 §6.1, #417). Counts every suppressed push site: WS playout, barge-in re-queues, the room mix, parked MOH and announcements. A sustained rate means the WS server keeps streaming through peer holds instead of pausing on the §3.3 `hold` event — harmless (the frames are dropped here, discarded, never queued) but wasted bandwidth. |
| `siphon_ai_notify_total`                | counter   | `result=accepted\|ignored\|bad_event\|bad_request` | Inbound NOTIFYs answered (#357). `accepted` = `Event: refer` (post-REFER transfer progress, RFC 3515) → `200 OK`, dropped without WS surfacing; `ignored` = `Event: message-summary`, a registrar's unsolicited MWI push (RFC 3842) → `200 OK` and discarded, since a bridge has no mailbox to display (#486); `bad_event` = an event package we don't support → `489 Bad Event`; `bad_request` = no `Event` header → `400`. **`bad_event` is the actionable label and should stay at zero** — it means a peer expects a subscription package (`dialog`, `presence`, …) SiphonAI doesn't implement. Expect `ignored` to track your REGISTER rate exactly (one per refresh) on any node registered to a mailbox-enabled PBX: that is FreeSWITCH's and Asterisk's default behaviour, it is harmless, and it is deliberately kept out of `bad_event` so the alertable signal stays clean. |
| `siphon_ai_session_refresh_total`       | counter   | `result=ok\|rejected\|failed`         | RFC 4028 session refreshes SiphonAI sent on an **outbound** leg whose callee nominated it as refresher (#484). `ok` = 2xx, the armed expiry is pushed out; `rejected` = the peer answered a non-2xx final response (`422 Session Interval Too Small`, `503`, or a `408`/`481` saying the dialog is gone); `failed` = no usable response at all (timeout / transport error). Absent rather than zero on most deployments: SiphonAI never refreshes an inbound leg (it nominates the caller), and outbound legs only refresh when the callee nominates us, which is the SBC-shaped minority. |
| `siphon_ai_session_refresh_stopped_total` | counter | `reason=dialog_gone\|exhausted\|unresolvable` | Outbound refresh loops that stopped while the call was still up (#484). **Alert on any increment**: it means nothing is keeping that session alive, so the call will end at the armed RFC 4028 deadline. `dialog_gone` = the peer answered `408`/`481` (terminal on the first occurrence — retrying cannot resurrect a dead dialog, RFC 3261 §12.2.1.2); `exhausted` = consecutive failures hit the give-up threshold; `unresolvable` = the local dialog handle disappeared, normally teardown winning the race. The loop deliberately does **not** BYE the call — RFC 4028 §10 suggests it, but that is the deployment's decision, so it stops and reports instead. Pair with `siphon_ai_calls_total{cause="local_shutdown"}` and a duration close to the negotiated `Session-Expires` to confirm the deadline did the ending. **Alert on `siphon_ai_session_refresh_total{result!="ok"}` too, not only on this counter.** Refreshes run five seconds ahead of `Session-Expires/2` while the expiry sits a full `Session-Expires` past the last success, so two attempts fit inside a dying session and the second lands ten seconds clear of the deadline — enough for `exhausted` to fire before the expiry ends the call (on a 90 s timer: attempts at t+40 and t+80, teardown at t+90). Before #490 the period was exactly `Session-Expires/2`, which put that second attempt *on* the deadline: `exhausted` usually lost the race and never reached the counter. `dialog_gone` remains the branch that fires deterministically (first `408`/`481`). A single non-`ok` refresh already means the session is in danger. |
| `siphon_ai_outbound_calls_active`       | gauge     | —                                     | In-flight outbound calls (admitted but not yet settled). Compare with `[outbound].max_concurrent`. |
| `siphon_ai_outbound_srtp_total`         | counter   | `result=encrypted\|downgraded`        | Outbound SRTP (SDES) outcomes for answered calls through a gateway with `[[gateway]].srtp` set (0.7.x). `encrypted` = trunk accepted SRTP; `downgraded` = `preferred` gateway, trunk answered plaintext (call continued unencrypted). A `required` trunk that refuses SRTP fails the call (`outbound_calls_total{result="failed"}`). |
| `siphon_ai_delayed_offer_total`         | counter   | `result=answered\|ack_timeout\|missing_sdp_answer\|invalid_sdp_answer\|no_compatible_codec\|invalid_remote_media\|caller_hangup` | Inbound delayed-offer (offerless INVITE) outcomes (0.9.0). `answered` = the ACK's SDP answer negotiated and the call bridged; `ack_timeout` = no ACK before SIP Timer H (~32 s); `missing_sdp_answer` = ACK had no body; `invalid_sdp_answer` = ACK body unparseable; `no_compatible_codec` = the answer picked nothing we offered; `invalid_remote_media` = the answer's RTP address/port was unusable or the stream was rejected; `caller_hangup` = the peer BYE'd between our 200-with-offer and the ACK answer (#425 — the CDR reports the matching `caller_hangup` cause). (Pre-answer outbound failures ride `siphon_ai_outbound_calls_total`; outbound delayed-offer *negotiation* outcomes have their own counter below, #406.) |
| `siphon_ai_outbound_delayed_offer_total` | counter  | `result=answered\|srtp_policy\|srtp_setup\|invalid_remote_media\|media_activate\|missing_sdp_offer` | Outbound delayed-offer (offerless INVITE we sent; the peer offers in its 2xx, we answer in the ACK) negotiation outcomes (#406). `answered` = our ACK answer built and media bridged; `srtp_policy` = the gateway's `srtp` mode refused every audio alternative the peer offered; `srtp_setup` = the selected secure alternative failed to negotiate or install (bad crypto/fingerprint, post-process, DTLS enable); `invalid_remote_media` = the peer's 2xx offer was unusable (parse/codec failure); `media_activate` = the answer was built but forge refused to start RTP forwarding (#414); `missing_sdp_offer` = the 2xx carried no usable SDP offer. Pre-2xx failures (busy, 4xx, unreachable) ride `siphon_ai_outbound_calls_total`. |
| `siphon_ai_call_duration_seconds`       | histogram | —                                     | Wall-clock duration of ended calls, inbound and outbound (#373 — previously inbound-only). For outbound legs this spans originate→end, matching the CDR's `duration_ms` (ring time included; billable time is `ended_at − answered_at` on the CDR). |
| `siphon_ai_sdp_negotiate_seconds`       | histogram | `result=ok\|error`                    | Time spent in `prepare_call` (negotiate + port alloc + tap attach). |
| `siphon_ai_ws_connect_seconds`          | histogram | —                                     | WS handshake time. |
| `siphon_ai_register_state{name,state}`  | gauge     | `name`, `state=pending\|registered\|auth_failed\|rejected\|failed\|disabled` | Current row per `[[register]]`. Exactly one state per `name` is `1` at any time. |
| `siphon_ai_register_attempts_total`     | counter   | `name`, `outcome=registered\|auth_failed\|rejected\|transport_error` | One tick per REGISTER attempt. |
| `siphon_ai_metrics_requests_total`      | counter   | `result=ok\|unauthenticated`          | `/metrics` scrape outcomes, emitted **only when** `[observability].metrics_token` is set (0.35.0) — the series existing at all means the gate is on. A rising `unauthenticated` is a misconfigured scraper or a prober; rejected scrapes also log a rate-limited warning (≤1/min). |
| `siphon_ai_error_ring_captured_total`   | counter   | `level=warn\|error`                   | `warn!`/`error!` tracing events captured into the recent-errors ring (`GET /admin/v1/errors`, 0.49.0). A rate spike is a health signal in its own right — alert on it even if nobody is watching the ring. Counts what the global log filter passes; `error_ring_size = 0` stops storage but this still counts captures attempted. |
| `siphon_ai_sip_ring_messages_total`     | counter   | `result=captured\|dropped_call_cap\|dropped_trace_cap` | SIP messages offered to the per-call ladder ring (`GET /admin/v1/calls/{id}/sip`). A rising `dropped_call_cap` says `sip_ring_max_messages` is wrong for this deployment, or something is retransmitting. `dropped_trace_cap` counts whole dialogs evicted by the pending bound — REGISTER/OPTIONS/scanner traffic crowding out live calls; a sustained rate there means the ladder may miss a call you go looking for. |
| `siphon_ai_sip_ring_traces`             | gauge     | —                                     | SIP dialogs currently held by the ladder ring: live calls, retained completed calls, **and non-call dialogs** (REGISTER refreshes, OPTIONS, rejected INVITEs) that also carry a `Call-ID`. Named `traces` rather than `calls` for exactly that reason. Bounded by `sip_ring_size` completed plus 256 pending. |
| `siphon_ai_ws_failure_prompts_total`    | counter   | `result=played\|cut_short\|unusable\|timeout` | WS-failure prompt playbacks (0.34.0, `[bridge].on_ws_failure = "play_prompt"`). `played` = EOF reached; `cut_short` = caller hangup/teardown preempted it; `unusable` = the file failed to load at call time (fell open to plain hangup — check the rate matches the bridge); `timeout` = the 30 s safety cap fired (config smell: prompt too long, warned at load). |
| `siphon_ai_register_admin_triggers_total` | counter | `name`, `action=refresh\|restart`     | Operator registration triggers **accepted** by the admin API (0.33.0). The resulting REGISTER's outcome lands on `siphon_ai_register_attempts_total{name,outcome}` as usual — a trigger with no matching attempt tick means the command was coalesced behind an already-queued one. |
| `siphon_ai_barge_in_decisions_total`    | counter   | `outcome=confirmed\|rejected\|timeout` | Pause-mode barge-in arbitration resolutions (0.32.0, `[bridge.barge_in].mode = "pause"`). `confirmed` includes server verdicts *and* preempting commands (mute/hold/park/`clear`/conference-join/WS drop); `timeout` = the `on_timeout` fallback ruled. A high `timeout` share means the server isn't sending verdicts (or `decision_ms` is too tight); a high `rejected` share means VAD false positives are being caught — working as designed. |
| `siphon_ai_barge_in_decision_seconds`   | histogram | —                                     | Arbitration latency: armed on `speech_started` → resolved. Explicit buckets 50ms–5s; the ceiling of the distribution is the configured `decision_ms` (timeout resolutions land there). |
| `siphon_ai_silence_events_total`        | counter   | —                                     | Times `silence_detected` fired on the WS bridge. Configurable via `[bridge].silence_threshold_ms`. |
| `siphon_ai_dead_air_events_total`       | counter   | —                                     | Times `dead_air_detected` fired on the WS bridge. Configurable via `[bridge].dead_air_threshold_ms`. |
| `siphon_ai_rtp_jitter_ms`               | histogram | —                                     | RTP jitter snapshot recorded on every `rtp_stats` emission (when forge has reported a value). Explicit buckets 1 ms – 500 ms. |
| `siphon_ai_rtp_packet_loss_ratio`       | histogram | —                                     | Packet-loss ratio (0.0-1.0) recorded on every `rtp_stats` emission. Explicit buckets 0.001 – 1.0 (a fraction, not a percentage). |
| `siphon_ai_rtp_rtt_ms`                  | histogram | —                                     | RTCP-derived round-trip time (ms) per received Receiver Report (RFC 3550 §A.7). Populated since 0.3.2 (forge originates SRs); explicit buckets 10ms–1s. Records a sample roughly every RTCP cycle (~5s) once bidirectional RTCP is flowing. |
| `siphon_ai_rtp_rx_jitter_ms`            | histogram | —                                     | Locally-measured interarrival jitter (RFC 3550 §6.4.1) on the caller→SiphonAI stream, recorded on every `rtp_stats` emission once local media-stats snapshots exist (0.30.0). The receive-side counterpart of `siphon_ai_rtp_jitter_ms` (which is remote-reported); same buckets, 1 ms – 500 ms. |
| `siphon_ai_rtp_mos_estimate`            | histogram | —                                     | Transport-only MOS-CQE estimate (1.0–5.0), simplified E-model over local RX jitter/loss + RTCP RTT, recorded on every `rtp_stats` emission once RX data exists (0.30.0). Same math heplify-server applies to HEP QoS chunks. Buckets cut on the conventional quality bands (2.6 / 3.1 / 3.6 / 4.0). |
| `siphon_ai_sip_tls_reload_attempts_total` | counter | `outcome=ok\|failed`                  | One tick per SIGHUP cert-reload attempt. `failed` means a broken cert/key on disk; the listener keeps serving the previous cert. |
| `siphon_ai_admin_tls_reload_attempts_total` | counter | `outcome=ok\|failed`                | Same as above for the `[admin.tls]` listener cert (0.18.0). One tick per SIGHUP admin-cert reload; `failed` keeps the previous cert. Only emitted when `[admin.tls]` is configured. |
| `siphon_ai_config_reloads_total`        | counter   | `result=applied\|no_change\|failed`   | SIGHUP config-file reloads (0.12.0). `applied` = a changed config loaded and the hot-reloadable sections (routes, webhook/CDR sinks) were swapped; `no_change` = the file was byte-identical to the last load; `failed` = the new config didn't load/compile and the running config was kept. Alert on `failed` after a deploy. |
| `siphon_ai_conference_joins_total`      | counter   | `result=joined\|disabled\|too_many_rooms\|room_full\|rate_mismatch\|already_joined\|error` | Conference joins attempted (0.7.0). Every non-`joined` row leaves the call on its direct caller↔WS pair. |
| `siphon_ai_conferences_active`          | gauge     | —                                     | Live conference rooms (0.7.0). A room spawns on first join and exits when its last member leaves. |
| `siphon_ai_conference_participants`     | gauge     | —                                     | Mixer participants across all rooms (0.7.0). Each member call contributes 2 — its SIP leg and its WS session; two calls in one room read 4. |
| `siphon_ai_room_tick_lag_seconds`       | histogram | —                                     | How far past its 20 ms cadence a room's mix tick fired (0.7.0). Healthy rooms sit in the lowest bucket; sustained lag means the mixer (which allocates per tick upstream — DEV_PLAN_0.7.0.md §6) or the runtime is starved. Buckets 0.5 ms – 250 ms. |
| `siphon_ai_room_frames_dropped_total`   | counter   | `stage=input\|sink`, `side=sip\|ws` | 20 ms frames a room dropped instead of blocking the audio path (0.7.0). `input` = the tap→room channel was full; `sink` = a member's output channel was full (stalled consumer). Healthy rooms sit at zero. |
| `siphon_ai_parks_total`                 | counter   | `result=ok\|rejected`                 | Park requests (0.7.0). `rejected` = park disabled or `[park].max_parked` reached; the call continues unparked. |
| `siphon_ai_retrieves_total`             | counter   | `result=ok\|not_parked`               | Retrieve requests (0.7.0). `not_parked` = a retrieve signalled a call that wasn't parked (ignored). |
| `siphon_ai_parked_calls_active`         | gauge     | —                                     | Currently-parked calls (0.7.0). Incremented on park, decremented on retrieve or teardown. |
| `siphon_ai_holds_total`                 | counter   | `result=ok\|failed`                   | Bot-initiated hold/resume re-INVITEs (0.7.2 — the WS server sends `hold`/`resume`). Covers both directions. `failed` = the re-INVITE was rejected / timed out / glared, or hold was rejected (already held by the far end, tap unavailable, not configured); the call stays in its prior media state. Does **not** count far-end (peer-initiated) holds. |
| `siphon_ai_ws_reconnects_total`         | counter   | `result=recovered\|exhausted`         | WS reconnect episodes mid-call (0.7.3 — `[bridge].ws_reconnect_enabled`). One increment per unexpected drop that entered the reconnect path. `recovered` = re-dialed the same `ws_url` within `ws_reconnect_max_secs`; `exhausted` = the window elapsed (or the call ended mid-gap) and the call tore down (`ws_disconnect`). |
| `siphon_ai_admin_requests_total`        | counter   | `endpoint`, `role`, `result=ok\|unauthenticated\|forbidden\|not_found\|error` | Admin API requests on the `[admin]` listener (0.10.0). `endpoint` is the bounded route template (e.g. `POST /admin/v1/calls`, ids collapsed to `:id`), `role` is the authenticated token's role (`none` for `unauthenticated`). `unauthenticated` = 401 (missing/bad token); `forbidden` = 403 (role below the endpoint minimum); `not_found` = 404 (unknown route **or** a handler acting on a stale call/room/binding id); `error` = any other handler failure (400 / 409 / 429 / 501 / 503). `result` follows the response status, so `result != "ok"` is a true failure count (fixed in 0.37.1 — it previously flattened all authorized responses to `ok`). Pair with the structured audit log (actor = token name) for per-request detail. |
| `siphon_ai_webhook_deliveries_total`    | counter   | `sink=lifecycle\|cdr\|audit\|quality`, `result=delivered\|spooled\|rejected\|dropped` | Terminal webhook/CDR/audit/quality delivery outcomes (0.11.0; `audit` added 0.20.0, `quality` 0.31.0). One increment per logical delivery. `delivered` = 2xx; `spooled` = persisted to the durable spool after the in-memory budget; `rejected` = non-retryable 4xx; `dropped` = budget (or spool) exhausted, or payload not serializable. |
| `siphon_ai_quality_records_total`       | counter   | `kind=interval\|final`               | Quality history records emitted through the `[quality]` sinks (0.31.0). Skipped-empty records don't count. |
| `siphon_ai_webhook_delivery_attempts_total` | counter | `sink`, `outcome=ok\|transient\|error\|rejected` | Individual HTTP delivery attempts (0.11.0) — a retried delivery ticks several times. `transient` = retryable 5xx/408/429; `error` = connect/timeout; `rejected` = non-retryable 4xx. Divide by `siphon_ai_webhook_deliveries_total` for attempts-per-delivery. |
| `siphon_ai_webhook_spool_depth`         | gauge     | `sink`                                | Deliveries waiting in the durable spool (0.11.0, set when `spool_dir` is configured). Sampled by the drain worker each pass (self-correcting across restarts). Healthy = 0; a rising value means deliveries are failing and backing up on disk. |
| `siphon_ai_recording_uploads_total`     | counter   | `result`                              | Recording uploads to object storage (0.25.0): `ok` (durable), `failed` (will retry), `dropped` (retry budget exhausted / recording gone — stays local-only). |
| `siphon_ai_recording_upload_spool_depth`| gauge     | —                                     | Recording uploads waiting in `[recording.storage].spool_dir`. Healthy = 0; rising means the object store is unreachable. |
| `siphon_ai_recording_upload_seconds`    | histogram | —                                     | Wall-time of one successful recording upload. |
| `siphon_ai_dialogs_active`             | gauge     | —                                     | SIP dialogs held in the shared `DialogManager` (0.48.13). Sampled by the dialog reaper each sweep, so it self-corrects rather than relying on paired increments. Should track `siphon_ai_calls_active`, lagging it by the reaper's 32 s grace window — a finished dialog is kept briefly so a retransmitted BYE still matches. **Climbing with cumulative calls instead of settling is issue #458**: dialogs were inserted per call and never removed, and `sip-dialog` caps the store at 10,000, after which `insert` fails silently and in-dialog requests stop matching. Alert if it exceeds a few times your steady-state concurrency. |
| `siphon_ai_sip_rate_limited_total`      | counter   | `transport=udp\|tcp\|tls\|ws\|wss`    | Inbound SIP packets/frames dropped by the **per-source-IP ingress rate limit** (`[sip].udp_rate_limit_pps` / `stream_rate_limit_fps`, 0.48.11). Present from startup. This limiter sits **below `[sip.admission]`** and applies even when admission control is off — the drop happens in the transport before the packet is parsed, so nothing else in the daemon can see it. Any sustained movement means real SIP is being discarded: raise the limit, or split the source. The source IP is deliberately not a label (unbounded cardinality, and spoofable on UDP) — the throttled `sip_transport` WARN names the peer. |
| `siphon_ai_hep_packets_sent_total`      | counter   | —                                     | HEP3 packets written to the wire (`[hep]`). Mirrored every 10 s from the sink's own counter, so it lags a scrape slightly; present from startup. **Wire-level success only, and not a liveness signal** — a black-holing NAT or a heplify-server that isn't storing still counts here, and against a *dead* collector the counter keeps climbing at roughly half rate (connected-UDP reports the refusal on the following send, so alternate sends succeed). Alert on the `hep_rs::udp` WARN for that; see `docs/HEP.md`. |
| `siphon_ai_hep_packets_dropped_total`   | counter   | `reason=queue_full`                   | HEP3 packets the producer dropped before the wire because `[hep].queue_capacity` (default 256) was full — the daemon emitting faster than the worker ships, per CLAUDE.md §4.7. Sustained movement means raise the capacity; it does **not** mean the collector is down. One label value today; `collector_down` would need a send-failure counter in `hep-rs` (#460). |
| `siphon_ai_otlp_log_records_dropped_total` | counter | `reason=queue_full`                 | Log records dropped before the OTLP exporter (`[observability.otlp.logs]`, 0.51.0). The emitting task hands each record to a bounded queue and returns; a worker thread drains it into the SDK. A full queue means the export path is not keeping up with the log rate, and the record is discarded rather than allowed to hold up the task that raised it (CLAUDE.md §4.7). **Movement here means the console has lines the collector never received.** Sustained movement usually means `[observability.otlp.logs].level` is too broad for the link, not that the daemon is unhealthy. As with HEP, a `collector_down` label value is deliberately left room for. |
| `siphon_ai_webhook_delivery_seconds`    | histogram | `sink`                                | Delivery latency in seconds, accepted → 2xx (0.11.0). Includes spool dwell, so a slow/recovered receiver shows as a fat tail. Buckets 5 ms – 30 s. |
| `siphon_ai_draining`                    | gauge     | —                                     | `1` while the daemon is draining for shutdown (0.17.0, `[shutdown]`), `0` otherwise. Set the instant a SIGTERM/SIGINT drain begins — new INVITEs are then `503`'d and `/ready` reports not-ready. A scraper that catches `1` knows the pod is going away. |
| `siphon_ai_drain_seconds`               | histogram | —                                     | How long the shutdown drain took (0.17.0): drain start → registry empty or the `[shutdown].drain_timeout_secs` deadline. Observed once per process lifetime, so only a scrape catching the dying pod (or a push gateway) sees it. Buckets 0.1 s – 120 s. A value near the timeout means calls didn't finish in the window. |
| `siphon_ai_calls_drain_forced_total`    | counter   | —                                     | Calls force-terminated (BYE + WS hangup) at the drain deadline (0.17.0) — they were still active when `[shutdown].drain_timeout_secs` elapsed. `0` after a clean rolling deploy; non-zero means the drain window was too short for the call mix. Also appear on `siphon_ai_calls_total{cause="drain_forced"}` and per-call on the CDR. |
| `siphon_ai_webrtc_legs_total`           | counter   | `codec=opus\|pcmu\|pcma\|other`, `result=connected\|ice_timeout\|dtls_timeout\|failed\|closed` | Browser media legs by what they negotiated and how setup ended (`[webrtc]`). **The ice/dtls split is the diagnosis**: `ice_timeout` means `[webrtc].setup_timeout` expired with *no* candidate pair — there is no path, so look at NAT, at `[webrtc].stun_servers`, and at whether `[media].rtp_port_range` is actually open through the firewall (a browser leg binds inside that range). `dtls_timeout` means a pair *was* nominated and the handshake still did not finish: the path works and the crypto does not. `failed` is forge-webrtc giving up on the transport; `closed` is the tab going away mid-setup, which is a user action rather than a fault. The `codec` label is what makes `siphon_ai_webrtc_transcode_seconds` readable — Opus and G.711 cost an order of magnitude apart. |
| `siphon_ai_webrtc_legs_ended_total`     | counter   | `reason=peer_closed\|transport_failed\|inactivity\|send_failed\|controller` | Browser legs that ended **after** connecting. `peer_closed` and `controller` are normal call endings (the browser closed the transport, or we did — BYE, WS hangup, shutdown). **`inactivity` is the consent-freshness signal**: RFC 7675 consent failure is not surfaced as an event by forge-webrtc, so a browser that vanishes without a BYE — closed tab, closed laptop — is detected by `[media].inactivity_timeout_secs` firing on silence, exactly as on a SIP leg. A rising `inactivity` rate means pages are disappearing rather than hanging up; it is also the thing that gives the call slot (and its RTP port pair) back, so a *stuck* value here with `siphon_ai_calls_active` high is worth alerting on. |
| `siphon_ai_webrtc_ice_seconds`          | histogram | —                                     | Browser leg media start → ICE nominating a candidate pair. Host candidates on a LAN land in the bottom bucket; above a second means STUN, a long candidate list, or a slow browser. Legs that never nominate are **not** in here — they are `siphon_ai_webrtc_legs_total{result="ice_timeout"}`, so this histogram's count is deliberately not the browser-call count. |
| `siphon_ai_webrtc_dtls_seconds`         | histogram | —                                     | The DTLS handshake itself: ICE nomination — where forge-webrtc starts it — → SRTP keys installed. Separate from the ICE histogram because a handshake is round trips over a path ICE already proved works, so the two phases slow down for unrelated reasons. |
| `siphon_ai_webrtc_transcode_seconds`    | histogram | `direction=decode\|encode`            | Wall time one browser leg spent inside the codec, recorded once when the leg ends. Codec work is pure computation with no I/O, so this is CPU time in all but name — the capacity number for browser traffic. It excludes SRTP protect and the socket write (transport cost, not transcode cost). Divide by `siphon_ai_call_duration_seconds` for the fraction of a core a leg burns, and read it next to the `codec` label above: Opus costs far more than G.711, which is what `[webrtc].prefer_g711` exists to trade away. |
| `forge_*`                               | various   | per-call (forge-side)                 | Media internals from the embedded forge crates, exported through this daemon's recorder. See forge-media's `docs/METRICS.md` for the inventory. The two forge histograms this daemon can emit — `forge_vad_neural_inference_seconds` (`vad = "neural"` routes) and `forge_transcoding_duration_seconds` — render with forge-engine's suggested buckets, applied by our exporter (#437); every other reachable forge family is a counter or gauge. |
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
