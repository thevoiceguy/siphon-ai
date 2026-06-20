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
- **Secret-manager integration** — beyond `${VAR}`: systemd credentials /
  file-based secrets / Vault, so tokens and passwords needn't sit in the
  environment.

(The optional `/metrics` bearer token moved to P2 — it's recon-hardening,
not a PII fix.)

### Inbound security — don't trust the network
Today the only inbound gate is the `[[trunk]]` allowlist, and `from_hosts`
matching is spoofable by an on-path attacker. Two complementary additions for
internet-facing daemons (especially trunks without a static carrier IP):

- **Inbound digest auth (RFC 3261 §22)** — challenge inbound INVITEs with a
  digest credential. `docs/CONFIG.md` already names this the proper
  "no trust in network" answer and marks it post-v1.
- **Per-source INVITE rate limits + admission control** — a DoS posture
  beyond the allowlist, complementing the existing fail2ban recipe.

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

## P1 — Recording: compliance & storage

Recording (0.5.0) writes a plaintext WAV to a local dir — fine for a lab,
short of what regulated industries (PCI/HIPAA/call-center) need. The gaps are
documented in `docs/RECORDING.md`.

- **Encryption at rest** — envelope encryption with a KMS hook (the
  compliance blocker).
- **Object-storage sink** — S3-compatible upload + a retention policy
  (lifecycle/TTL), instead of local-disk-only.
- **Consent / announcement hooks** — a configurable "this call may be
  recorded" prompt before capture starts.
- **Compression & format** — Opus / FLAC output (smaller than WAV), plus
  path templating (`{date}/{call_id}` etc.).
- **Outbound recording** — extend capture to outbound legs (currently
  inbound-only).

---

## P1 — Protocol SDKs & machine-readable schemas

The WS protocol (`docs/PROTOCOL.md`) is the product contract, but every
integrator hand-rolls JSON + 20 ms audio framing from prose. Lower the
barrier and make the contract testable:

- **JSON Schema** for every WS message (generated from / checked against the
  Rust types in `crates/bridge`).
- **Server SDKs** — TypeScript and Python, handling framing, audio
  endianness/rate, and the message envelope, so a bot author writes handlers
  not wire code.
- **Conformance suite + protocol testkit** — a harness that replays a
  scripted call against a candidate WS server and validates its responses,
  plus a local mock daemon for SDK CI.

---

## P1 — Per-call quality telemetry (live + history)

`/metrics` is aggregate and HEP is operator-side; there's no first-class way
to feed a per-call view into the operator's own dashboards. Complements the
CDR quality fields below (CDR = end-of-call record; this = the live stream +
queryable history).

- **Live per-call stats** — richer `rtp_stats` (jitter/loss/RTT, audio
  timing, barge-in) streamed over the WS and/or an out-of-band webhook during
  the call.
- **History** — a queryable store / export so dashboards can chart per-call
  quality over time, not just scrape Prometheus aggregates.

---

## P2 — High-value call features

- **AMD (answering-machine / voicemail detection)** — human-vs-machine on
  answered outbound calls, surfaced as a WS event. Needs a `forge-amd`
  sibling to `forge-vad` (*upstream-gated*).
- **WS-failure prompt playback** — on a WS failure, play a configurable prompt
  before teardown instead of only `hangup`.

---

## P2 — Security & CDR (small, high-signal)

- **Optional `/metrics` bearer auth** — token on `/metrics` only, off by
  default; `/health`+`/ready` stay open. Defense-in-depth for deployments
  that expose the observability port widely. (Confirmed `/metrics` carries no
  PII — only aggregate counters + operator-chosen route/register names — so
  this is recon-hardening, not a PII fix.)
- **CDR call-quality fields** — add `first_audio_out_ms` (bridge-connected →
  first WS audio reaching the caller) and `barge_in_count`; both are flagged
  as gaps in `docs/OPERATIONS.md`. Small, high-signal; bumps `CDR_VERSION`.
  (The delayed-offer setup-failure CDR already shipped in 0.9.5.)

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
