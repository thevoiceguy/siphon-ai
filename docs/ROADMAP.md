# SiphonAI Roadmap

The original v1 development plan (`docs/DEV_PLAN.md`) is **complete**, and
the post-plan themes have shipped through **v0.32.0**. The product is
feature-complete on the *call-handling* surface — codecs (G.711/Opus), SRTP
(SDES + DTLS, both directions), hold/transfer/conference/park, outbound
origination, recording (encrypted, S3, consent), reversible barge-in, WS
reconnect, and STIR/SHAKEN verification — and both P0s plus **every P1
theme** below are delivered: release & packaging (0.16.0), graceful shutdown
(0.17.0), security & abuse hardening (0.18–0.20), observability completeness
(0.21–0.23), recording compliance & storage (0.24–0.26), protocol SDKs &
schemas (0.27–0.29), and per-call quality telemetry (0.30/0.31).

What's left is mostly **P2 call features and operator conveniences, plus the
longer-horizon P3 items**. This document is the curated, prioritized backlog.
Items are grouped by priority; within a theme the work follows the usual
cadence (design note → locked decisions → chunked PRs → tag-after-merge).
Anything marked *upstream-gated* depends on a capability in `siphon-rs` /
`forge-media` that doesn't exist yet.

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

### Listener & secret hardening — ✅ delivered in v0.18.0
- **`[admin].tls`** — native TLS on the admin listener, with SIGHUP cert
  reload.
- **Secret resolution** — `${file:…}` / `${cred:…}` (systemd credentials)
  beyond `${VAR}`, so tokens and passwords needn't sit in the environment.

(The optional `/metrics` bearer token moved to P2 — it's recon-hardening,
not a PII fix.)

### Inbound security — ✅ delivered in v0.19.0
- **Inbound digest auth (RFC 3261 §22)** — `[sip.auth]` challenges inbound
  INVITEs with a digest credential; the proper "no trust in the network"
  answer for trunks without a static carrier IP.
- **Per-source INVITE rate limits + admission control** — `[sip.admission]`,
  complementing the `[[trunk]]` allowlist and the fail2ban recipe.

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
- **OpenTelemetry / OTLP traces** — daemon-side export ✅ **delivered in
  v0.22.0** (`[observability.otlp]`): one OTLP trace per call across the
  daemon (INVITE handling → controller → WS bridge → media), off by default,
  best-effort. W3C trace-context propagation to the WS server
  (`start.trace_context` + upgrade headers) ✅ **delivered in v0.23.0** —
  the developer's server joins the same trace. **Theme complete.**

---

## P1 — Recording: compliance & storage — ✅ delivered in v0.24.0–v0.26.0

All five gaps closed (design note `DESIGN_RECORDING_COMPLIANCE.md`):

- **Encryption at rest** — ✅ v0.24.0: `.wava` envelope encryption + a
  decrypt CLI.
- **Object-storage sink** — ✅ v0.25.0: S3-compatible upload with an
  AWS-KMS KEK (no AWS SDK dependency).
- **Consent / announcement hooks** — ✅ v0.26.0: pre-capture announcement +
  a CDR consent stamp.
- **Compression & format** — ✅ v0.25.0: Opus output.
- **Outbound recording** — ✅ v0.26.0: outbound legs record like inbound.

---

## P1 — Protocol SDKs & machine-readable schemas

The WS protocol (`docs/PROTOCOL.md`) is the product contract, but every
integrator hand-rolls JSON + 20 ms audio framing from prose. Lower the
barrier and make the contract testable:

- **JSON Schema** for every WS message — ✅ **delivered in v0.27.0**
  (`schemas/siphon-ai.v1.json`, generated from the Rust types in
  `crates/bridge`, drift-checked + docs-corpus-validated in CI).
- **Server SDKs** — ✅ **delivered in v0.28.0** (`sdks/python` +
  `sdks/typescript`: typed events, paced 20 ms framing, close semantics;
  schema/corpus-validated in CI; echo examples rewritten on them).
- **Conformance suite + protocol testkit** — ✅ **delivered in v0.29.0**
  (`siphon-ai-testkit`: TOML-scripted calls against a candidate WS server,
  schema-validating assertions, JSON report; the `conformance` CI job runs
  it against both SDK echo servers). **Theme complete** — see the
  retrospective in `docs/design/DESIGN_PROTOCOL_SDKS.md` §8.

---

## P1 — Per-call quality telemetry (live + history) — ✅ delivered in v0.30.0/v0.31.0

- **Live per-call stats** — ✅ **v0.30.0**: richer `rtp_stats` (locally
  measured RX counters + a transport MOS estimate) on the WS stream, plus
  the CDR `quality` block (CDR v4).
- **History** — ✅ **v0.31.0**: `[quality]` per-call history records
  (file/webhook sinks), `GET /admin/v1/calls/{id}/stats` live snapshot, and
  a Vector→Loki dashboard pipeline. Theme complete — see
  `docs/design/DESIGN_QUALITY_TELEMETRY.md`.

---

## P2 — High-value call features

- **Reversible barge-in** — ✅ **delivered in v0.32.0** (added to this list
  retroactively; it emerged from field feedback on false barge-ins).
  `[bridge.barge_in].mode = "pause"`: playout is flushed instantly but the
  unplayed tail is retained, and the WS server rules on intent via
  `barge_in_confirm`/`barge_in_reject` — a rejected false positive (cough,
  backchannel) resumes the bot mid-utterance. See
  `docs/design/DESIGN_REVERSIBLE_BARGE_IN.md`. Two follow-ups remain below.
- **Neural VAD upgrade in forge-vad** (*upstream-gated*) — a Silero-class
  local model (small ONNX, ~1 ms/frame CPU) to cut the acoustic
  false-positive class (coughs, keyboard noise, music) before pause-mode
  arbitration even arms. Complements — doesn't replace — the semantic
  layer. Gate on real-call false-positive rates under `pause` + `debounce_ms`.
- **"Duck" barge-in reaction** (*upstream-gated*) — attenuate instead of
  pause. Needs a forge-media per-leg playout-gain API: the queued TTS tail
  lives in forge's encoder queue, so tap-side gain can't touch it (the
  reason v0.32.0 shipped pause, not duck).
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
- **CDR call-quality fields** — ✅ **delivered in v0.30.0**:
  `first_audio_out_ms` and `barge_in_count` shipped in the CDR `quality`
  block (CDR v4), closing the `docs/OPERATIONS.md` Q5/Q8 gaps; v0.32.0
  added the optional barge-in arbitration counters alongside them.

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
