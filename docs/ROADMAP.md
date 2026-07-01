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

### Release & packaging — ✅ delivered in v0.16.0
*Was the most-flagged drift.* Shipped a tag-triggered release workflow
(`.github/workflows/release.yml`, design note
`docs/design/DESIGN_RELEASE_PACKAGING.md`, runbook `RELEASING.md`):

- Prebuilt multi-arch (`x86_64` + `aarch64`, musl-static) binaries on each
  GitHub release, cross-compiled with cargo-zigbuild.
- Automated `tag → build → SBOM (syft) → checksums + cosign-keyless
  signatures → GitHub release → multi-arch GHCR container (cosign-signed)`.
- `.deb` packages (`amd64` + `arm64`) via cargo-deb.
- **CI version-consistency gate** (`scripts/check-version-consistency.py`) —
  fails the build if `Cargo.toml`, `CHANGELOG.md`, and `README.md` disagree.

  *Still open (deferred, low priority):* `.rpm` packages and a distroless /
  hardened container variant — both structured as additive follow-ups (a
  sibling packaging step / a swapped runtime base), not rework.

### Graceful shutdown & zero-drop deploys — ✅ delivered in v0.17.0
*Was the remaining open P0.* Shipped over three chunks (design note
`docs/design/DESIGN_GRACEFUL_SHUTDOWN.md`). Combined with `SIGHUP` reload this
unlocks true rolling deploys:

- On `SIGTERM`/`SIGINT`: flip `/ready` to not-ready, reject new INVITEs with
  `503` + `Retry-After`, let active calls finish (bound by
  `[shutdown].drain_timeout_secs`, default 30 s), then force-terminate
  stragglers at the deadline with a real `BYE` + WS `hangup`. A second signal
  forces an immediate exit.
- Drain status on `GET /admin/v1/drain` + metrics (`siphon_ai_draining`,
  `siphon_ai_drain_seconds`, `siphon_ai_calls_drain_forced_total`) + a
  `drain_forced` CDR cause (CDR v3).

*With both P0s delivered, the next theme is P1 (security & abuse hardening).*

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

### Signed audit-event stream — ✅ delivered in v0.20.0
Admin requests were logged + metered, but there was no tamper-evident,
shippable audit trail. (Explicitly deferred from the admin-auth theme.)

- **Delivered:** `[audit]` — admin/security events (`admin_request`,
  `sip_auth`, `invite_rejected`, `attestation_rejected`, `config_reload`,
  `cert_reload`) emitted to an append-only JSONL file and/or an HMAC-signed
  webhook for SIEM ingestion, reusing the 0.11.0 signer. A HEP audit stream
  remains a possible follow-up. **This completes the P1 security & abuse
  hardening theme** — only STIR/SHAKEN outbound signing (below) remains, and
  it's scoped as its own demand-gated effort.

### STIR/SHAKEN outbound signing
SiphonAI *verifies* inbound attestation (0.4.0) but cannot *sign* outbound
calls. Regulatory-driven for some operators; large.

- ES256 PASSporT signing + `Identity` header injection on outbound. Needs
  STI-CA cert enrollment (a multi-week external process) — gate on demand.

---

## P1 — Observability completeness

The metrics/logs/HEP primitives are rich; the gap is shipped consumer
artifacts and distributed tracing. Scoped in
[`docs/design/DESIGN_OBSERVABILITY.md`](design/DESIGN_OBSERVABILITY.md).

- **Dashboards & alerts as code** — ✅ **delivered in v0.21.0.** Runnable
  Prometheus + Grafana stack in [`examples/observability/`](../examples/observability/)
  (recording + alerting rules + Fleet Overview / Call Quality dashboards),
  `docs/OPERATIONS.md` "ten questions" made concrete, and an anti-drift CI
  check keeping metric names honest.
- **OpenTelemetry / OTLP traces** — today tracing is structured logs + HEP
  correlation only; OTLP spans would let operators trace a call across the
  daemon + their WS server in one view. Next up: v0.22.0 (daemon-side export)
  then v0.23.0 (W3C trace-context propagation to the WS server). Deps + the
  additive protocol change are approved (DESIGN_OBSERVABILITY §5).

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

## P2 — Registration management (admin)

Today a `[[register]]` row only re-REGISTERs on its own refresh timer (or a
daemon restart). When an upstream (e.g. CUCM) drops or stales a binding,
operators have no way to force it back without bouncing the daemon — which
interrupts active calls. Extends the existing read-only `GET
/admin/registrations` (0.10.0) on the authenticated `[admin]` listener with
two write actions (operator role), neither of which touches media:

- **`POST /admin/registrations/{name}/refresh`** — fire an immediate
  authenticated REGISTER for one binding, off-cycle, without a restart.
  Implementation: give each registration task a `Notify` (or a small command
  channel) and have its loop select over all three wake sources —

  ```rust
  tokio::select! {
      _ = tokio::time::sleep(refresh_delay)   => {}  // normal cadence
      _ = refresh_signal.notified()           => {}  // operator-triggered
      _ = shutdown_signal.cancelled()         => return,
  }
  ```

  so a refresh just nudges the existing loop rather than spawning anything.

- **`POST /admin/registrations/{name}/restart`** — full unregister/register
  cycle: REGISTER with `Expires: 0` to clear the binding, then a fresh
  authenticated REGISTER. For when a refresh isn't enough (server-side stale
  state, contact rebinding after an IP change).

Both return the resulting registration state (reuse the `GET` snapshot
shape). `404` for an unknown `{name}`; bounded-cardinality metric on
trigger; audit-logged (actor = token name) like every other admin write.
Per-binding only — no global "refresh all" in the first cut (operators can
script the list).

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
- Marker-bit fix — pending forge-media PR, then bump the pins.

---

## Permanently out of scope

Not roadmap items — deliberate non-goals (`CLAUDE.md` §8, `DEV_PLAN.md` §1):
AI-provider code (the WS server's job), multi-tenancy, video, WebRTC client
support, YAML/JSON config, in-daemon acoustic echo cancellation, per-call log
files.
