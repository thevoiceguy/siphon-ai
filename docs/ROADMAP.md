# SiphonAI Roadmap

The original v1 development plan (`docs/DEV_PLAN.md`) is **complete**, and
the post-plan themes shipped through **v0.12.1**: admin auth + RBAC (0.10.0),
webhook/CDR delivery durability (0.11.0), and the config CLI + `SIGHUP`
reload (0.12.0/0.12.1). The product is feature-complete on the *call-handling*
surface — codecs (G.711/Opus), SRTP (SDES + DTLS, both directions),
hold/transfer/conference/park, outbound origination, recording, WS reconnect,
and STIR/SHAKEN verification.

What's left is mostly **operational maturity, security hardening, and a few
high-value call features** — not core bridging. This document is the curated,
prioritized backlog. Items are grouped by priority; within a theme the work
would follow the usual cadence (design note → locked decisions → chunked PRs →
tag-after-merge). Anything marked *upstream-gated* depends on a capability in
`siphon-rs` / `forge-media` that doesn't exist yet.

---

## P0 — Production operability

These are the gaps that bite real deployments today.

### Release & packaging
*The most-flagged drift.* Releases are hand-cut and there are no installable
artifacts beyond from-source scripts + a Docker image.

- Prebuilt, multi-arch release binaries attached to each GitHub release
  (x86_64 + aarch64, musl static).
- Automated release workflow: tag → build → SBOM → checksums/signatures →
  GitHub release → container push to GHCR.
- Native packages: `.deb` (Debian/Ubuntu) at minimum; `.rpm` as a stretch.
- **CI version-consistency gate** — fail the build if `Cargo.toml`,
  `CHANGELOG.md`, and `README.md` disagree on the version / status (this
  repo has already drifted twice).
- Distroless / hardened container variant.

### Graceful shutdown & zero-drop deploys
The daemon aborts its listeners on `SIGTERM`; in-flight calls are not drained
(`runtime.rs` explicitly notes "v1 doesn't have a 'drain calls cleanly'
path"). Combined with `SIGHUP` reload this unlocks true rolling deploys.

- On `SIGTERM`: stop accepting new INVITEs, let active calls finish, bound by
  a configurable drain timeout, then exit.
- A `503`/`486` posture for new inbound during drain; `/ready` flips to
  not-ready so a load balancer stops routing.
- Drain status surfaced on `/admin` + a metric.

---

## P1 — Security & abuse hardening

### Listener & secret hardening *(bundle)*
- **`[admin].tls`** — native TLS on the admin listener (today it's plain HTTP,
  loopback-or-front-with-proxy only).
- **Optional `/metrics` auth** — bearer token on `/metrics` only, off by
  default; `/health`+`/ready` stay open. (Parked 2026-06-19 — confirmed
  `/metrics` carries no PII, only aggregate counters + operator-chosen route/
  register names, so this is recon-hardening, not a PII fix.)
- **Secret-manager integration** — beyond `${VAR}`: systemd credentials /
  file-based secrets / Vault, so tokens and passwords needn't sit in the
  environment.

### Inbound abuse protection
Today the only inbound gate is the `[[trunk]]` allowlist. There's no
rate-limiting or flood protection on the INVITE path.

- Per-source INVITE rate limits + global admission control (a DoS posture for
  internet-facing daemons), complementing the existing fail2ban recipe.

### Signed audit-event stream
Admin requests are logged + metered, but there's no tamper-evident, shippable
audit trail. (Explicitly deferred from the admin-auth theme.)

- Emit admin/security events as a signed webhook / HEP stream for SIEM
  ingestion (reuse the 0.11.0 HMAC signer).

### STIR/SHAKEN outbound signing
SiphonAI *verifies* inbound attestation (0.4.0) but cannot *sign* outbound
calls. Regulatory-driven for some operators; large.

- ES256 PASSporT signing + `Identity` header injection on outbound. Needs
  STI-CA cert enrollment (a multi-week external process) — gate on demand.

---

## P1 — Observability completeness

The metrics/logs/HEP primitives are rich, but there are no shipped consumer
artifacts and no distributed tracing.

- **Dashboards & alerts as code** — shipped Grafana dashboard JSON +
  Prometheus recording/alerting rules + the `docs/OPERATIONS.md` "ten
  questions" runbook made concrete.
- **OpenTelemetry / OTLP traces** — today tracing is structured logs + HEP
  correlation only; OTLP spans would let operators trace a call across the
  daemon + their WS server in one view.

---

## P2 — High-value call features

- **Outbound call recording** — recording (0.5.0) is inbound-only; extend it
  to outbound legs.
- **AMD (answering-machine / voicemail detection)** — human-vs-machine on
  answered outbound calls, surfaced as a WS event. Needs a `forge-amd`
  sibling to `forge-vad` (*upstream-gated*).
- **WS-failure prompt playback** — on a WS failure, play a configurable prompt
  before teardown instead of only `hangup`.

---

## P2 — Media & encryption depth

- **Mid-call SRTP re-key** — periodic/triggered re-key. *Upstream-gated*:
  forge-media has no coordinated re-key API (deferred since 0.5.0).
- **Fuller NAT traversal (ICE/STUN)** — v1 does symmetric-RTP + a static
  `public_address` SDP override; full ICE is post-v1.

---

## P3 — Scale & protocol (longer horizon)

- **Horizontal scale story** — the architecture is share-nothing per call, so
  N daemons behind a SIP load balancer should "just work"; this is about
  validating + documenting a blessed topology (call distribution, shared
  registration, aggregated observability) rather than new core code.
- **WS protocol v2** — collect the additive-vs-breaking wishlist (binary
  framing efficiency, richer codec negotiation, multi-stream). The v1 contract
  has held since 0.1.0; v2 is only worth it once there's a breaking need.

---

## Small follow-ups (low effort, do opportunistically)

- `print-config --format json` (config CLI nicety).
- CSV CDR output (alongside JSON/JSONL).
- Per-route `[route.bridge.tls]` override (global mTLS shipped in 0.3.0).
- Marker-bit fix — pending forge-media PR, then bump the pins.

---

## Permanently out of scope

Not roadmap items — deliberate non-goals (`CLAUDE.md` §8, `DEV_PLAN.md` §1):
AI-provider code (the WS server's job), multi-tenancy, video, WebRTC client
support, YAML/JSON config, in-daemon acoustic echo cancellation, per-call log
files.
