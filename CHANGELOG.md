# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.30.0] - 2026-07-13

### Added

- **Local receive-side RTP stats on `rtp_stats`** (P1 "Per-call quality
  telemetry", release 1 of 2). The `rtp_stats` WS event was
  remote-reported only — RTCP Receiver Reports describing how the far
  end hears the stream SiphonAI *sends*. It now also carries the side
  SiphonAI *receives*, measured locally by forge-media
  (`MediaStatsSnapshot`, forge-media#81; pin bumped to `5fa76fb38675`):
  additive optional `rx_jitter_ms` (RFC 3550 §6.4.1 interarrival
  jitter at the negotiated RTP clock), and cumulative
  `rx_packets_received` / `rx_packets_lost` (sequence-gap transit
  loss; late arrivals repair it) / `rx_packets_out_of_order` /
  `rx_packets_duplicate`. A congested path is often asymmetric —
  the two viewpoints on one event tell "they hear us badly" from
  "we hear them badly". The WS protocol stays **v1**; schema
  regenerated and both server SDKs updated in lockstep.
- **`mos_estimate`** on `rtp_stats`: transport-only MOS-CQE in
  `[1.0, 5.0]` via the simplified E-model over local RX jitter/loss
  plus RTCP RTT — the same math heplify-server applies to SiphonAI's
  HEP QoS chunks, so Homer-side and WS-side scores agree. `null`
  until RX data exists.
- **CDR `quality` block** — **CDR `version` 3 → 4** (additive-optional
  block; bumped per the 0.9.5 new-block precedent). Per-call summary
  in the record operators already ingest: `first_audio_out_ms` (WS
  `start` on the wire → first server audio frame reaching playout —
  the STT/LLM/TTS first-token latency; closes OPERATIONS.md Q5),
  `barge_in_count` (`auto_clear` firings + server `clear` commands;
  closes Q8), `avg/max_jitter_ms`, `avg/max_packet_loss_ratio`,
  `avg_rtcp_rtt_ms` (RTCP-RR aggregates), end-of-call `rx_packets_*`
  totals, and `mos_estimate_min/avg`. Unmeasured fields are omitted,
  not zeroed; the block is omitted entirely for calls that never went
  active.
- **Metrics**: `siphon_ai_rtp_rx_jitter_ms` and
  `siphon_ai_rtp_mos_estimate` histograms, recorded on every
  `rtp_stats` emission once RX data exists.

### Changed

- The daemon now configures forge-media to publish local media-stats
  snapshots at a fixed 5 s cadence (RTCP-conventional). They feed both
  the `rtp_stats` `rx_*` fields and the CDR `quality` block, so the
  CDR populates even on routes with WS `rtp_stats` emission disabled.
  Cost: one broadcast event per receiving leg per 5 s.

## [0.29.0] - 2026-07-10

### Added

- **Protocol conformance testkit — `siphon-ai-testkit`** (P1 "Protocol
  SDKs & machine-readable schemas", final release — the theme is
  complete). A new `crates/protocol-testkit` binary that plays the
  *daemon's* side of WS protocol v1 against any candidate server — no
  SIP, no RTP, no daemon needed. Scripted calls from TOML scenarios
  (five bundled: `basic-echo`, `dtmf`, `recording-controls`,
  `hangup-semantics`, `keepalive`; `--scenario-dir` adds your own) with
  every server message validated against `schemas/siphon-ai.v1.json`
  **and** the daemon's real wire types, exact 20 ms frame sizing and
  real-time pacing asserted, §5.7 close semantics enforced (bare close
  mid-call is a violation; server `hangup` is honored daemon-style),
  unknown-event tolerance probed, and WS keepalive checked. Exit code 0
  iff conformant plus a JSON report (`--report`) — *"conformant with
  protocol v1"* is now a claim any third-party server's CI can gate on.
  See `docs/CONFORMANCE.md`.
- **`conformance` CI job** — every PR now runs the full scenario set
  against **both** SDK echo servers (`echo-ws-server-python`,
  `echo-ws-server-node`) — the first CI coverage for the Node echo
  server, closing the theme's last verification gap.

The WS protocol stays **v1**; the daemon binary is unchanged (the
testkit's one new dependency, the `jsonschema` validator, is
test-tooling only).

## [0.28.0] - 2026-07-10

### Added

- **Server SDKs — `sdks/python` + `sdks/typescript`** (P1 "Protocol SDKs
  & machine-readable schemas", second release). Two dependency-light
  packages (`siphon-ai-server`; `websockets` / `ws` respectively) that
  implement the WS bridge protocol so a bot author writes handlers, not
  wire code: WS accept with `siphon-ai.v1` subprotocol echo, typed events
  for all 21 daemon→server messages, one `Call` method per server→daemon
  command (all 17), a **paced 20 ms audio re-framer** (arbitrary byte
  pushes → exact 320/640 B frames at real time — the code every example
  hand-rolled), §5.7 close semantics (`hangup` vs bare-close drop), and
  `start.reconnected` surfaced. Zero AI dependencies. Types are
  hand-written and **validated against `schemas/siphon-ai.v1.json` plus
  every `docs/PROTOCOL.md` example** in each SDK's test suite, with full
  union coverage asserted — a new `sdk-tests` CI job runs both suites on
  every PR. Vendorable (`pip install ./sdks/python`,
  `npm install ./sdks/typescript`); registry publishing deferred.
- **`examples/echo-ws-server-node`** — new minimal echo server on the
  TypeScript SDK.

### Changed

- **`examples/echo-ws-server-python` is rewritten on the Python SDK**
  (566 → 408 lines, same CLI and behavior, every `--auto-*` test-harness
  knob kept). It remains the SIPp CI fixture, so every daemon PR now
  exercises the Python SDK end-to-end against real calls.

The WS protocol stays **v1**; the daemon binary is unchanged.

## [0.27.0] - 2026-07-09

### Added

- **Machine-readable protocol schema — `schemas/siphon-ai.v1.json`** (P1
  "Protocol SDKs & machine-readable schemas", first release; design note
  `docs/design/DESIGN_PROTOCOL_SDKS.md`). The complete WS protocol
  contract as JSON Schema draft 2020-12, **generated from the Rust wire
  types** in `crates/bridge`: `$defs/BridgeOut` (21 daemon→server
  messages) + `$defs/BridgeIn` (17 server→daemon), doc comments as
  descriptions, and an `x-binary-frames` annotation describing the audio
  half (raw PCM16-LE, 320 B @ 8 kHz / 640 B @ 16 kHz, 20 ms). Point your
  editor, validator, or code generator at it. The top level is `anyOf`
  (not `oneOf`): `hold`/`resume`/`mark` exist in both directions, so
  validate against the direction-specific union when you know who sent
  the frame. A new CI gate regenerates the schema and diffs it on every
  PR, **and validates every JSON example in `docs/PROTOCOL.md` against
  it** (39 today) — the protocol docs, Rust types, and schema can no
  longer drift apart silently. Generation is behind a dev-only
  `json-schema` cargo feature (`schemars`); the daemon binary is
  unchanged. Protocol stays **v1**.

## [0.26.0] - 2026-07-09

### Added

- **Recording consent announcement — `[recording.announcement]`** (P1
  "Recording compliance & storage", final release — the theme is
  complete). Point `file` at a "this call may be recorded" WAV and the
  daemon plays it to the caller right after answer; **capture starts only
  when the prompt finishes**. The WS session connects in parallel
  (announce-then-bridge); the bot can't talk over the prompt, and nothing
  the caller says during it reaches the recording *or* the server. With
  `mode = "on_demand"`, a `start_recording` arriving mid-prompt is
  deferred to prompt completion. **Fail-closed**: if the prompt can't play
  (missing file, wrong sample rate), the call is *not* recorded — and the
  CDR shows `consent.announced = false`. Applies to inbound and outbound
  legs. **Off by default.**
- **Consent audit trail on the CDR** — additive
  `consent { announced, announcement_ms, server }` object (schema version
  unchanged). `announced`/`announcement_ms` come from the daemon-played
  prompt; `server` from the new **`set_recording_consent`** WS control
  message (`{ "type": "set_recording_consent", "call_id", "note"? }`) —
  a stamp for consent your server captured itself (DTMF press-1, verbal
  yes). A stamp, not a gate: capture gating stays `on_demand` +
  `start_recording`. Protocol stays **v1**.
- **Outbound-leg recording.** Originated calls (`POST /admin/v1/calls`)
  can record exactly like inbound ones — same `[recording]`
  dir/encryption/format, same on-demand WS controls, same object-storage
  upload spool. Per-gateway default (`[[gateway]].recording = "off"
  (default) | "always" | "on_demand"`, validated at load) plus a
  per-originate `"recording"` override (`400` for bad values, rejected
  before a toll-fraud concurrency permit is consumed). Recording an
  outbound leg is config/API opt-in, never implied. **Off by default.**

## [0.25.0] - 2026-07-08

### Added

- **Object-storage upload — `[recording.storage]`** (P1 "Recording
  compliance & storage", second release). Finalized recordings upload to
  any S3-compatible bucket (AWS, MinIO, Cloudflare R2, Backblaze B2 —
  path-style, hand-rolled SigV4, **no AWS SDK**). Durable by design: a
  small job file lands in `spool_dir` at call teardown (atomic, survives
  restarts) and a background worker uploads with retries; a job that keeps
  failing is dropped with a metric rather than wedging the spool, and the
  local file is deleted only after a durable upload (opt-in
  `delete_local_after_upload`). `key_template` names objects with
  `{call_id}` / `{date}` / `{route}` / `{direction}`. The CDR gains an
  additive `recording_url` (`s3://bucket/key`, stamped at enqueue) and a
  new **`recording_uploaded`** lifecycle webhook (after `call_end`)
  confirms arrival with `size_bytes`. New metrics:
  `siphon_ai_recording_uploads_total{result}`,
  `siphon_ai_recording_upload_spool_depth`,
  `siphon_ai_recording_upload_seconds`. Retention/TTL stays the bucket
  lifecycle policy's job (worked recipe in `docs/RECORDING.md` §9). Pair
  with `[recording.encryption]` so the bucket only ever holds ciphertext.
  **Off by default.**
- **AWS KMS as the recording KEK — `[recording.encryption.kms]`**. The KMS
  hook the 0.24.0 envelope design reserved: each recording's data key is
  wrapped by KMS `Encrypt` (the KEK never exists outside KMS; every unwrap
  is IAM-auditable), on the same SigV4 client — still no AWS SDK. Exactly
  one of `kek` / `kms`; `endpoint` override supports KMS-compatible
  emulators. `siphon-ai decrypt-recording` gains `--kms-region` /
  `--kms-endpoint` (credentials via `AWS_ACCESS_KEY_ID` /
  `AWS_SECRET_ACCESS_KEY`); symmetric-KMS blobs name their own key, so no
  key ARN is needed to decrypt. **Off by default.**
- **Ogg-Opus recording format — `[recording].format = "opus"`**. ~10×
  smaller than WAV for voice, encoded with the same libopus the media path
  already uses and playable by ffmpeg/VLC/browsers. Streaming-native
  (RFC 7845), so nothing needs a finalize back-patch — including inside an
  encrypted envelope. Extensions: `.opus` plaintext, `.opusa` sealed.
  Adds the `ogg` crate (the theme's one new small dependency). **Default
  stays WAV.**

## [0.24.0] - 2026-07-08

### Added

- **Recording encryption at rest — `[recording.encryption]`** (P1 "Recording
  compliance & storage", first sub-item; design note
  `docs/design/DESIGN_RECORDING_COMPLIANCE.md`). With `enabled = true`, a
  `kek` (64 hex chars, referenced via `${file:}`/`${cred:}`) and a `key_id`,
  recordings are written as encrypted **`.wava` envelopes** instead of
  plaintext WAV — nothing plaintext ever touches disk. Envelope encryption:
  a fresh random 256-bit data key per recording seals the audio in
  independent AES-256-GCM chunks; the data key travels in the file header,
  wrapped by your KEK. The header names the `key_id` that wrapped it, so
  **rotating the KEK never re-encrypts audio**. Config is validated
  fail-loud at startup; a runtime wrap failure fails the *recording*
  (`recording_failed`), never the call. The CDR gains an additive
  `recording_encrypted` flag (schema version unchanged). Decrypt offline
  with the new **`siphon-ai decrypt-recording <file> --kek-file <hex>`**
  subcommand — needs no daemon config; a wrong key names the `key_id` the
  recording requires; `--allow-unfinalized` recovers a crashed capture. The
  `SAIWAVA1` container format is documented in `docs/RECORDING.md` §8 for
  third-party implementations. **Off by default.** Deps: `aes-gcm` +
  `zeroize` promoted from transitive to direct (RustCrypto; no new vendor).

### Changed

- **Recordings now appear as `<name>.part` while in progress** and are
  renamed to their final `.wav`/`.wava` name only when finalized — for
  *plaintext* recordings too. A bare `.wav` on disk is now always a
  complete file (safe for a watcher/uploader to pick up), and a daemon
  crash leaves only a `.part` instead of a WAV with placeholder header
  sizes. **If you watch the recording directory, match the final names and
  ignore `*.part`.**

## [0.23.0] - 2026-07-08

### Added

- **W3C trace-context propagation to the WS server** (P1 "Observability
  completeness"; final sub-item — the theme is complete). When
  `[observability.otlp]` is enabled, the WS upgrade request now carries
  [`traceparent`](https://www.w3.org/TR/trace-context/) (+ `tracestate` when
  non-empty), and the `start` message carries the same values in a new
  additive `trace_context` field for servers whose WS library hides upgrade
  headers. A WS server that continues the trace from either place appears in
  the **same waterfall** as the daemon's SIP/media spans — one distributed
  trace per call across both services. The span-id propagated is the daemon's
  call-root span; park-retrieve and WS-reconnect sessions stay in the same
  trace. **The protocol stays v1**: the field is absent whenever OTLP is
  disabled (the default), so existing servers see an unchanged `start` shape.
  No new knob — OTLP on ⇒ headers + field, off ⇒ neither. The reference echo
  and OpenAI-Realtime example servers show the continuation pattern. See
  `docs/PROTOCOL.md` §3.1 and `docs/CONFIG.md` → `[observability.otlp]`.

### Fixed

- **OTel span-context extraction now reaches the OTLP layer.** The 0.22.0
  init installed the OTLP tracing layer behind `tracing_subscriber::reload`,
  whose downcast barrier made the layer invisible to
  `OpenTelemetrySpanExt::context()` — span *export* worked, but anything
  asking a live span for its trace context got nothing (this would have
  silently disabled 0.23.0's propagation). The layer is now installed
  concrete with a reloadable per-layer filter (`OFF` until `[observability.otlp]`
  activates it), preserving the zero-cost-when-disabled property.

## [0.22.0] - 2026-07-03

### Added

- **OpenTelemetry / OTLP distributed tracing — `[observability.otlp]`** (P1
  "Observability completeness"; second sub-item of the theme). Export
  per-call traces over OTLP/gRPC to a collector (Tempo / Jaeger / an
  OpenTelemetry Collector). Each call is **one trace** — `on_invite →
  on_matched → accept_inbound → run → { WS bridge, media }` — with the SIP
  `Call-ID`, direction, and from/to on the root span, so an operator can see
  where a call spent its time across the daemon. Config knobs: `endpoint`
  (default `http://localhost:4317`), parent-based `sample_ratio`,
  `timeout_ms`, `service_name`, and extra resource `attributes`; independent
  of the metrics HTTP listener (traces without metrics scraping is a valid
  setup). **Off by default** and **best-effort** (CLAUDE.md §4.7): spans batch
  on a background worker and drop on overflow, so a slow or unreachable
  collector never blocks a call; a bad endpoint fails loud at startup, a
  collector that's merely down does not. When disabled the tracing layer is a
  zero-cost no-op. Pending spans flush on shutdown. See `docs/CONFIG.md` →
  `[observability.otlp]`. W3C trace-context propagation to the WS server is a
  follow-up (v0.23.0).

## [0.21.1] - 2026-07-01

### Fixed

- **SIP-over-TCP/TLS trunks no longer wedge after ~60s of a call**
  (CUCM and any persistent-connection trunk). The SIP stack closed an
  inbound TCP/TLS connection after 60s with no inbound SIP — but a trunk
  keeps its signaling connection open for a call's whole life while
  sending **no SIP at all** (RTP is out-of-band), so 60s idle was hit by
  essentially every call. The reaped connection then dropped mid-call
  re-INVITEs and BYEs (they got no response — the socket was gone before
  the transaction layer saw them), leaving the peer's dialogs stuck and
  its trunk health-check failing → `503` on new calls. The idle timeout
  is now two-phase: a short Slowloris window until a connection completes
  its first SIP message, then a long, configurable **established** timeout
  (new `[sip].tcp_idle_timeout_secs`, default `1800`; `0` disables). UDP
  is connectionless and was never affected. Requires the paired siphon-rs
  transport fix (bumped here). See `docs/CONFIG.md` → `[sip]`.

## [0.21.0] - 2026-07-01

### Added

- **Dashboards & alerts as code** (P1 "Observability completeness"; first
  sub-item of the theme). A runnable Prometheus + Grafana stack under
  [`examples/observability/`](examples/observability/) — the consumer
  artifacts for the metrics the daemon already emits, no daemon code. Ships
  a reference scrape config, **16 recording rules** (per-route call rates,
  INVITE reject ratio, latency percentiles for WS-connect / SDP-negotiate /
  call-duration / RTP-RTT / packet-loss / room-tick-lag, webhook delivery
  success ratio, registration state), **12 alerting rules** (target/
  registration down, high reject rate, dead air, slow WS connect, high RTP
  RTT / packet loss, spool backlog, delivery failing, admission flooding,
  sip-auth brute force, drain forced), and **two provisioned Grafana
  dashboards** (Fleet Overview + Call Quality). `docker compose -f
  examples/observability/compose.yaml up` stands the whole stack up.
- **Observability anti-drift CI check.** `scripts/check-observability-metrics.py`
  (new `observability artifacts` CI job) asserts every `siphon_ai_*` metric
  referenced in the shipped rules/dashboards is actually emitted by the
  daemon, and `promtool check config` validates the PromQL — so a metric
  rename can't ship silently-broken artifacts (same spirit as the version
  gate).

### Changed

- **`docs/OPERATIONS.md` made concrete.** The §11.8 "ten questions" now carry
  the worked PromQL and the covering dashboard/alert for each metrics-
  answerable one, plus a symptom → dashboard table. `docs/DEPLOY.md`'s metrics
  section points to the shipped stack. (Prometheus/Grafana for the aggregate;
  Homer for the individual call.)

## [0.20.0] - 2026-07-01

### Added

- **Signed audit-event stream — `[audit]`** (P1 "Security & abuse
  hardening"; the last sub-item of the theme). A tamper-evident trail of
  admin and security decisions for SIEM ingestion — *who did what* on the
  `[admin]` surface and *what the daemon refused* on the SIP surface —
  distinct from `[webhooks]` (ops automation) and `[cdr]` (billing).
  Ships to an append-only JSONL **file** (`[audit.file]`, for a log
  shipper) and/or an HMAC-signed **webhook** (`[audit.webhook]`, for a
  SIEM collector); enable either or both. The webhook reuses the 0.11.0
  delivery transport, so the `X-SiphonAI-Signature` HMAC (the
  tamper-evidence), `X-SiphonAI-Event-Id` idempotency, durable spool, and
  the `siphon_ai_webhook_*` delivery metrics (label `sink="audit"`) all
  behave identically. Six event types — `admin_request`, `sip_auth`,
  `invite_rejected`, `attestation_rejected`, `config_reload`,
  `cert_reload` — with an `events` allowlist. Emission is deliberately
  signal-first: `invite_rejected` records admission `rate_limited` /
  `no_trunk` / `draining` but **not** the per-packet silent flood-drop
  (auditing that DoS-shedding fast path would amplify the attack), and
  `sip_auth` records `failed` / `stale` but **not** the normal per-call
  `challenged` / `ok`. Off by default; hot-reloadable on `SIGHUP` when
  enabled at startup (enabling from off is restart-required). Best-effort
  and off the call path — a slow SIEM never blocks an admin request or a
  SIP transaction. New `docs/AUDIT.md`; see also `docs/CONFIG.md` →
  `[audit]`. Completes the P1 security & abuse hardening theme.

## [0.19.0] - 2026-06-27

### Added

- **Inbound INVITE admission control — `[sip.admission]`** (P1 "Security &
  abuse hardening"; second chunk of v0.19.0). A DoS posture beyond the
  `[[trunk]]` allowlist: shed abusive inbound INVITEs **before** any
  trunk / auth / route work. A **per-source token bucket** keyed on the
  source IP (`max_per_sec` + `burst`) answers an over-rate source `503` +
  `Retry-After`, and after `drop_after` consecutive rejects **silently
  drops** further INVITEs from it (an obvious flood doesn't earn a
  response). An optional **global `max_concurrent`** cap (read from the
  live call registry) answers `503` once the node is at capacity. Source
  buckets live in a size-capped table (`max_sources`) with idle/oldest
  eviction, so the limiter can't leak memory under a spoofed-source
  flood. New metrics
  `siphon_ai_invite_admission_total{result=accepted|rate_limited|dropped}`
  + `siphon_ai_invite_admission_sources` gauge. Off by default;
  restart-required on `SIGHUP` (part of `[sip]`). See `docs/CONFIG.md` →
  `[sip.admission]`.

- **Inbound digest authentication — `[sip.auth]`** (P1 "Security & abuse
  hardening"; first chunk of v0.19.0). Challenge inbound INVITEs with RFC 3261
  §22 / RFC 7616 digest auth, so trust no longer rests on a spoofable network
  identity (source IP / `From:` host). A new out-of-dialog INVITE that needs
  auth and arrives without a valid `Authorization` is answered `401
  Unauthorized` + `WWW-Authenticate` (nonce/realm/qop); the peer re-sends with
  a digest `response` verified against the configured credentials. Replay is
  bounded by a server nonce TTL (an expired nonce gets a `stale=true`
  re-challenge). Configured by `[sip.auth]` (`enabled`, `realm`, `algorithm` =
  MD5/SHA-256/SHA-512, `qop`, and `[[sip.auth.user]]` credentials) — passwords
  resolve via `${file:…}`/`${cred:…}` (v0.18.0). Digest is an **AND-gate with
  the `[[trunk]]` allowlist**, opt-in per trunk via `auth_required = true`, so
  a static-IP carrier that doesn't send credentials stays allowlist-only and
  isn't broken by enabling auth; with no trunks (legacy mode) every INVITE is
  challenged. New metric `siphon_ai_sip_auth_total{result=ok|challenged|failed|stale}`.
  Uses the upstream `sip-auth` server-side verifier (no siphon-rs change).
  Off by default; no protocol/CDR/schema break. See `docs/CONFIG.md` →
  `[sip.auth]`.

## [0.18.0] - 2026-06-26

### Added

- **Admin listener TLS — `[admin.tls]`** (P1 "Security & abuse hardening";
  second chunk of v0.18.0). The authenticated `[admin]` listener can now serve
  **HTTPS** directly, so the bearer token is encrypted on the wire on a
  routable bind without a TLS-terminating proxy. Set `[admin.tls].cert` +
  `.key` (both required when the table is present; missing/empty → fatal at
  load). The cert is loaded at startup (fail-loud) and **hot-reloaded on
  `SIGHUP`** alongside `[sip.tls]` — the next connection picks up the new cert,
  in-flight ones keep theirs, and a broken PEM keeps the previous cert
  (nginx-style). New metric `siphon_ai_admin_tls_reload_attempts_total`
  `{outcome=ok|failed}`. Without `[admin.tls]` a non-loopback bind still works
  but logs a sharpened startup warning (the token travels in the clear). See
  `docs/CONFIG.md` → `[admin.tls]`.

- **Secret resolution from files & systemd credentials** (P1 "Security &
  abuse hardening"; first chunk of v0.18.0). Config `${...}` references can now
  pull a secret from outside the process environment, so plaintext secrets
  needn't sit in env vars (visible in `/proc/<pid>/environ`, dumps, unit
  files). Two new source prefixes, usable anywhere `${VAR}` works:
  `${file:/path/to/secret}` (trimmed file contents — Docker/Kubernetes
  secrets, Vault-Agent templated files) and `${cred:NAME}`
  (`$CREDENTIALS_DIRECTORY/NAME` — systemd `LoadCredential=`). Same fail-loud
  pass as `${VAR}`: a missing file, unset `$CREDENTIALS_DIRECTORY`, or path
  traversal in a credential name fails the load. `${VAR}`/`${VAR:-default}`
  behaviour is unchanged (the `:-` default operator still wins, so
  `${file:-x}` stays an env reference). See `docs/CONFIG.md` → *Secrets &
  variable expansion*.

## [0.17.0] - 2026-06-25

### Added

- **Graceful shutdown & connection draining** (P0 "Production operability").
  On `SIGTERM`/`SIGINT` the daemon now **drains** instead of dropping calls
  mid-conversation: it flips `/ready` to not-ready, rejects new inbound
  INVITEs with `503 Service Unavailable` + `Retry-After` (so an upstream
  proxy/LB routes elsewhere), lets in-flight calls finish — bounded by
  `[shutdown].drain_timeout_secs` (default `30`; `0` = pre-0.17.0 immediate
  exit) — then **force-terminates any stragglers at the deadline with a real
  `BYE` + WS `hangup`** rather than a silent RTP stop. In-dialog requests
  (re-INVITE/ACK/BYE) for calls already up keep flowing so the drained calls
  aren't broken. A **second** shutdown signal during the drain forces an
  immediate exit (operator escape hatch). This is what makes zero-drop
  rolling deploys possible — pair `drain_timeout_secs` with the supervisor's
  kill grace (`terminationGracePeriodSeconds` / `TimeoutStopSec`). See
  `docs/design/DESIGN_GRACEFUL_SHUTDOWN.md` and `docs/DEPLOY.md` →
  *Graceful shutdown & rolling deploys*.
- **`[shutdown]` config table** with `drain_timeout_secs` (`docs/CONFIG.md`).
  Restart-required on SIGHUP (read once at startup).
- **`GET /admin/v1/drain`** — live drain status
  `{draining, active_calls, drain_timeout_secs, remaining_secs}` for deploy
  scripts to confirm a pod entered drain and watch the countdown (readonly
  role).
- **Drain observability:** `siphon_ai_draining` gauge (1 while draining),
  `siphon_ai_drain_seconds` histogram (how long the drain took), and
  `siphon_ai_calls_drain_forced_total` counter (calls force-ended at the
  deadline). Drain lifecycle logs throughout.
- **SIPp coverage:** a graceful-drain phase in `test-harness/sipp-scenarios`
  (`drain_graceful_bye.xml` + `drain_invite_503.xml`) asserts end-to-end that
  a deadline straggler gets a real BYE, a new INVITE mid-drain is 503'd, and
  the daemon exits within the window.

### Changed

- **CDR schema → version 3.** Adds the `drain_forced` `termination.cause`
  value (calls force-ended at the drain deadline), distinct from
  `local_shutdown`, so a deploy's forced terminations are attributable
  per-call. Also surfaced on `siphon_ai_calls_total{cause="drain_forced"}`.
  A new value in an existing enum field — no field added or removed.
- The systemd unit sketch (`docs/DEPLOY.md`) gains `TimeoutStopSec=40` so the
  default 30 s drain window fits inside systemd's stop timeout.

## [0.16.0] - 2026-06-24

### Added

- **Docs: installing from a release + a releasing runbook.**
  `docs/DEPLOY.md` gains an *Install from a release* section (verify
  checksums + cosign signature, then install the binary, the `.deb`, or the
  signed container), and a new top-level `RELEASING.md` documents the
  "bump, then tag and push" flow the workflow automates. Final chunk of the
  P0 "Release & packaging" theme.
- **Automated release workflow** (`.github/workflows/release.yml`). Pushing
  a `v*` tag now builds multi-arch static-musl binaries (`x86_64` +
  `aarch64`, cross-compiled with cargo-zigbuild), packages them as
  per-arch `.tar.gz`, emits a `SHA256SUMS`, and creates the GitHub release
  with notes extracted from `CHANGELOG.md` (pre-release tags like
  `v0.16.0-rc.1` are marked accordingly, never latest). A `preflight` job
  re-asserts tag == workspace version before anything is built. Second
  chunk of the P0 "Release & packaging" theme.
- **Debian packages** (`.deb` for `amd64` + `arm64`, via cargo-deb). Each
  release now ships installable packages built from the same prebuilt
  static binaries: they drop the binary at `/usr/bin/siphon-ai`, a default
  conffile at `/etc/siphon-ai/config.toml`, and a hardened systemd unit
  (enabled but **not** started — the default config has a placeholder
  `ws_url`), and create the `siphon-ai` service user + `/var/{lib,log}`
  dirs in the maintainer scripts. `apt install ./siphon-ai_*_amd64.deb`.
  Fourth chunk of the P0 "Release & packaging" theme.
- **Release supply chain: SBOM, signatures, and a published container.**
  Each release now ships a CycloneDX SBOM (syft), a cosign **keyless**
  signature over `SHA256SUMS` (`SHA256SUMS.cosign.bundle`, verifiable
  against the workflow's GitHub OIDC identity), and a multi-arch
  (`linux/amd64` + `linux/arm64`) container at
  `ghcr.io/thevoiceguy/siphon-ai:<tag>` (also cosign-signed; `:latest`
  only for final releases). The image is assembled from the same prebuilt
  static binaries that ship on the release — byte-identical, no recompile.
  Third chunk of the P0 "Release & packaging" theme.

### Changed

- **Docker dev image tracks the toolchain.** `docker/Dockerfile` now uses
  `rust:1.95-alpine` (matching `rust-toolchain.toml`) instead of the stale
  `rust:1.85` base, which sat below the workspace MSRV and only built
  because `rust-toolchain.toml` forced a 1.95.0 download on top of it.

- **CI: version-consistency gate.** A new `version consistency` job
  (`scripts/check-version-consistency.py`) fails the build if the
  workspace `Cargo.toml` version, the README "Current release" marker, and
  the `CHANGELOG.md` dated heading disagree — closing the drift that left
  the README at v0.12.2 while the latest tag was v0.15.0 (README corrected
  to v0.15.0). First chunk of the P0 "Release & packaging" theme
  (`docs/design/DESIGN_RELEASE_PACKAGING.md`).

## [0.15.0] - 2026-06-24

### Added

- **Per-route `[route.bridge.tls]` override** — a route can now carry its
  own mTLS client config for the WS leg (client cert/key + optional SPKI
  pin), e.g. a pinned internal handler alongside a publicly-trusted shared
  one. When present it **fully replaces** the global `[bridge.tls]` for
  matching calls; routes without it inherit the global. Compiled (cert/key
  loaded, pin parsed) at config load — a bad path fails at startup, not on
  the first matching call — and lives on `CompiledRoute`, so it swaps
  atomically with the route table on `SIGHUP` reload like the rest of
  `[route.bridge]`. The `routes` crate gains an internal `siphon-ai-bridge`
  edge (no new external crate, no cycle). `print-config` / `route-test`
  show whether a route's bridge mTLS is on. See `docs/DIALPLAN.md` §5.5.

## [0.14.1] - 2026-06-22

### Fixed

- **Delayed-offer and outbound calls never bridged audio** (no RTP in
  either direction). Every offer/answer media path — inbound delayed offer
  (offerless INVITE → offer in 200 OK → answer in ACK) and outbound
  origination — funnels through `MediaSetup::apply_answer`, which bound the
  codec + remote address and attached the tap but **never activated the
  forge session**. The session stayed in `Initializing`, so forge's RTP
  forwarding task was never spawned: nothing was decoded inbound or sent
  outbound. The tap still attached (its timers fired `rtp_stats` /
  `silence_detected`), which masked the dead media — and on inbound calls
  the v0.13.0 start-deadline then tore the call down with `server_too_slow`.
  `apply_answer` now activates the session (`Initializing → Active`, starting
  forwarding), mirroring what the early-offer inbound path already did via
  `start_session` before its 200 OK. Only the early-offer inbound path
  (INVITE-with-SDP) was unaffected, which is why forcing CUCM Early Offer /
  an MTP appeared to "fix" it. Regression test asserts the session reaches
  `Active` after `apply_answer`.

## [0.14.0] - 2026-06-20

### Added

- **Error-signaling: `rtp_timeout`, `audio_format`, `protocol_error`**
  (PROTOCOL.md §2.2 / §3.10) — the last three documented `error` codes the
  daemon detected (or could trivially detect) but never emitted. Closes the
  protocol doc↔impl drift (bug #4).
  - **`rtp_timeout`** — when the media inactivity watchdog fires (no inbound
    RTP for `[media].inactivity_timeout_secs`), the WS server is now told
    *why* (`error{rtp_timeout}` + `stop`) before the socket closes, instead
    of seeing a bare close.
  - **`audio_format`** — inbound binary frames are validated against the
    negotiated frame size (320 B @ 8 kHz, 640 B @ 16 kHz). A wrong-size frame
    is **dropped** (non-fatal) and reported via `error{audio_format}`,
    **rate-limited** to the first bad frame + at most one/sec. The call stays
    up — one malformed frame can't kill it; persistent failure is still
    caught by the dead-air / rtp watchdog.
  - **`protocol_error`** — malformed JSON, an unknown message `type`, or a
    `call_id` that doesn't match the connection now emits
    `error{protocol_error}` + `stop` before closing. A **definitive**
    teardown (new `DisconnectReason::ProtocolError`, not reconnect-eligible —
    a buggy server would just repeat the violation). Previously these
    conditions tore down silently and *were* reconnect-eligible.

## [0.13.0] - 2026-06-20

### Added

- **WS liveness: keepalive + start-deadline** (PROTOCOL.md §5.6 / §3.1) —
  two documented MUSTs that were never implemented, so a non-responsive
  WS server could wedge a live call indefinitely. Now:
  - **Keepalive** — SiphonAI sends a WS Ping every
    `[bridge].ws_ping_interval_secs` (default 15 s) and, if no Pong lands
    within `[bridge].ws_pong_timeout_secs` (default 10 s), treats the
    connection as half-open and drops the session
    (`error { code: "internal", message: "ws keepalive timeout" }`,
    best-effort). A keepalive timeout is reconnect-eligible when
    `[bridge].ws_reconnect_enabled` (0.7.3), else it tears the call down.
    Previously only a *total* TCP disconnect was detected — a hung server
    on a live socket was invisible.
  - **Start-deadline** — the WS server must send its first audio frame
    (or `hangup`) within `[bridge].server_start_deadline_secs` (default
    5 s) of `start`, else the call is torn down with
    `error { code: "server_too_slow" }` + `stop`. A definitive teardown
    (not reconnect-eligible — redialing a slow server wouldn't help).
  - All three knobs default to the spec values; `0` disables the
    corresponding guard. Daemon-wide `[bridge]` settings; applies to
    inbound, outbound, and reconnect/retrieve WS sessions alike.

### Changed

- Bumped the forge-media pin to `049a19983a95` (forge-media PR #76):
  `forge-core`'s `EventBus::publish` no longer logs a spurious `WARN` when
  it has no subscribers. Drops log noise on the per-call event path;
  logging-only, no API or behavior change.

## [0.12.2] - 2026-06-20

### Fixed

- **`siphon-ai check` silently swallowed config load-time warnings**
  (security-relevant). The read-only subcommands (`check` / `print-config`
  / `route-test`) installed no tracing subscriber, so compile-time
  `warn!`s — notably the **SRTP-master-key-in-cleartext** footgun (a
  gateway with `srtp != off` but a non-TLS transport) — were dropped,
  making the documented pre-deploy preflight strictly *less* informative
  than a real boot. Tracing is now installed before the read-only
  subcommands, so `check` surfaces exactly what the daemon prints at
  startup (then still reports `config OK` + exit 0, since these are
  warnings, not errors).
- **SIGHUP webhook/CDR-spool reload warning over-fired.** With a
  `spool_dir` configured, every reload logged "delivery changes require a
  restart" even when `[webhooks]` / `[cdr]` hadn't changed. The warning now
  fires only when the sink's own config actually changed (and the reload no
  longer needlessly rebuilds an unchanged sink).
- **`[media]` restart-required check missed codec / DTMF changes** (silent
  config drift). The reload's restart-required fingerprint hashed only
  `rtp_port_range` / `moh_file` / `srtp`, so changing `[media].codecs` (or
  any `[bridge]` default) and reloading was silently swallowed — not
  hot-applied and with no restart-required warning. The fingerprint now
  covers the full `[media]` block plus the bridge/codec defaults, so any
  such change surfaces as restart-required.

## [0.12.1] - 2026-06-19

### Added

- **SIGHUP outbound gateway hot-reload.** `systemctl reload siphon-ai` now
  also rebuilds and swaps the `[[gateway]]` set — add / remove / modify
  trunks without a restart. In-flight outbound calls keep the trunk
  (`OutboundOriginator`) they're on; new originations use the new set. The
  gateway table moved behind an `ArcSwap` in the outbound service; each
  reload mints fresh per-gateway UACs (stateless senders over the shared
  transaction manager). Requires outbound enabled and the `[outbound]`
  limits unchanged — `max_concurrent` / `rate_limit_per_sec` resize the live
  admission semaphore and stay restart-required (a reload that changes them
  warns and applies only the safe sections). Completes the `SIGHUP` reload
  surface started in 0.12.0.

## [0.12.0] - 2026-06-19

### Added

- **Config CLI subcommands.** The daemon gains read-only subcommands;
  running the daemon is unchanged (`siphon-ai --config X`, no subcommand).
  - `siphon-ai check --config X` — validate + compile and exit (no sockets,
    no runtime). Exit `0` + a one-screen summary if valid, `1` + the error
    on stderr otherwise. The CI / pre-deploy / pre-`systemctl reload`
    preflight. (Also fixes the documented-but-nonexistent `--check` flag in
    `contrib/README.md`.)
  - `siphon-ai print-config --config X [--show-secrets]` — render the
    effective compiled config (post-`${VAR}`, post per-route merge); secrets
    redacted by default.
  - `siphon-ai route-test --config X --to N [...]` — run the dialplan against
    a synthetic call (first-match-wins) and report the winning route (or
    `NO MATCH → 404`) + its effective bridge config.
- **`SIGHUP` config hot-reload.** `systemctl reload siphon-ai` re-reads the
  `--config` file and hot-applies the reload-safe sections **without dropping
  calls**: the **route table** (new INVITEs use the new dialplan; in-flight
  calls keep their match) and the **`[webhooks]` / `[cdr]` sinks** (rebuilt +
  swapped, unless a durable `spool_dir` is active for that sink — its drain
  worker can't be hot-swapped). The `[sip.tls]` cert reload (0.3.0) is folded
  into the same handler.
  - **Fail-safe:** a config that doesn't load/compile is logged + counted and
    the running config is kept — a bad edit can't take the daemon down.
  - **Restart-required sections** (`[sip]` listen/transports, `[node]`,
    `[media]`, `[observability]`, `[admin]`, `[hep]`,
    `[security.stir_shaken]`, and `[[gateway]]` — gateway hot-reload is a
    planned follow-up) are applied-by-restart; a reload that changes one logs
    a warning naming it and still applies the safe sections.
  - New metric `siphon_ai_config_reloads_total{result=applied|no_change|failed}`.

## [0.11.0] - 2026-06-19

### Added

- **Webhook & CDR delivery trust + durability.** The shared outbound HTTP
  transport (lifecycle webhooks **and** the CDR webhook) gains, all
  additively — bodies are unchanged, so the webhook and CDR schema
  `version`s are **not** bumped:
  - **Idempotency.** Every delivery carries `X-SiphonAI-Event-Id` (+ an
    `Idempotency-Key` alias) — a UUIDv4 stable across retries and any spool
    replay. Delivery is at-least-once; receivers dedupe on this id.
  - **Authenticity (opt-in `secret`).** When `[webhooks].secret` /
    `[cdr.webhook].secret` is set, deliveries carry
    `X-SiphonAI-Signature: t=<unix>,v1=<hex>` — HMAC-SHA256 over
    `"<unix>.<raw-body>"`. The timestamp is inside the signed string, giving
    the receiver replay protection from a freshness window. The secret is
    `${VAR}`-expanded and never logged.
  - **Durability (opt-in `spool_dir`).** When `[webhooks].spool_dir` /
    `[cdr.webhook].spool_dir` is set, a delivery that exhausts the in-memory
    retry budget is persisted to disk and re-attempted by a background
    worker that **resumes after a daemon restart** (spool-on-failure: the
    happy path stays zero-disk-I/O). Oldest-first with capped backoff; a
    `4xx` or poison entry is eventually discarded; a per-sink file cap bounds
    disk (dropping the newest, never evicting an already-persisted entry).
    The directory is created + write-probed at startup, so a bad path fails
    the daemon loudly (CLAUDE.md §4.6). Unset ⇒ today's best-effort behavior,
    unchanged.
  - **Delivery metrics.** `siphon_ai_webhook_deliveries_total{sink,result}`,
    `siphon_ai_webhook_delivery_attempts_total{sink,outcome}`,
    `siphon_ai_webhook_spool_depth{sink}` (gauge), and
    `siphon_ai_webhook_delivery_seconds{sink}` (histogram). `sink` ∈
    `lifecycle` | `cdr`.

  See `docs/DEPLOY.md` → *Webhook delivery: signing, idempotency, durability*
  (incl. a receiver verification snippet) and the `[webhooks]` /
  `[cdr.webhook]` config reference.

## [0.10.0] - 2026-06-19

### Added

- **Native admin authentication + RBAC.** `/admin/*` is now gated by
  bearer tokens with three nested roles — `readonly` ⊂ `operator` ⊂
  `admin`. Tokens are declared under a new `[admin]` config block
  (`[[admin.token]] { name, token, role }`), hashed (SHA-256) at load and
  compared in constant time; the secret is never logged. The
  endpoint→minimum-role map: `readonly` = all GET/list routes; `operator`
  = hangup, park/retrieve, conference create/end/add/remove; `admin` =
  **billable** origination (`POST /admin/v1/calls`), `PUT /admin/log`, and
  `POST /admin/hep/test`. Missing/invalid token → `401` (+
  `WWW-Authenticate: Bearer`); role below the minimum → `403`. Config is
  validated at load (CLAUDE.md §4.6): an `[admin]` block with no tokens, an
  empty/duplicate name, an empty secret, an unknown role, or an
  unparseable `listen` fails the daemon at startup.
- **Admin request audit + metric.** Every admin request emits a structured
  log line (actor = token **name**, role, endpoint template, result, peer
  — never the secret) and ticks
  `siphon_ai_admin_requests_total{endpoint, role, result}` (`result` ∈
  `ok` | `unauthenticated` | `forbidden` | `not_found`; `endpoint` is a
  bounded route template with ids collapsed to `:id`).

### Changed

- **BREAKING: `/admin/*` moved off the metrics listener.** Admin endpoints
  are now served **only** on the dedicated `[admin].listen`; the
  `[observability].http_listen` port serves just `/metrics`, `/health`,
  `/ready` and returns `404` for `/admin/*`. **Migration:** add an
  `[admin]` block with at least one token, repoint admin tooling at the new
  port with an `Authorization: Bearer …` header, and remove any `/admin/*`
  allow rules (or front-proxy auth) from the metrics port. If `[admin]` is
  omitted, `/admin/*` is **not served at all** (secure default) — the
  daemon still starts and serves metrics/health. The admin listener is
  plain HTTP for now (`[admin].tls` is a planned follow-up); bind it on
  loopback or front it with TLS termination. A non-loopback bind logs a
  warning at startup.

## [0.9.5] - 2026-06-19

### Fixed

- **Inbound delayed offer never bridged** (regression latent since 0.9.0).
  The daemon's packet pump special-cased ACK — it cleared the 200-OK
  retransmit timer and returned *without* dispatching the request to the
  UAS — so `on_ack` never fired and the delayed-offer call was never
  finalized from the ACK's SDP answer. Early-offer calls were unaffected
  (their ACK carries no body and needs no handling). The 200 OK with our
  offer was sent and the dialog looked up (so a BYE got a 200), which is
  why the SIPp tests — which only asserted the 200 OK content — missed
  it. Now a **body-carrying ACK is dispatched to the UAS** (`on_ack` →
  `finalize_delayed_offer` → bridge); body-less ACKs keep the
  timer-only fast path. The `delayed_offer` SIPp phase now also asserts
  the bridge actually connected.

### Added

- **Per-call CDR for delayed-offer negotiations that fail before going
  active.** A delayed-offer call whose ACK answer never arrives or is
  unusable (the 200-with-offer was sent but the call never reached a
  controller) now writes a CDR, not just a metric + log. Five new
  `TerminationCause` variants — `ack_timeout`, `missing_sdp_answer`,
  `invalid_sdp_answer`, `no_compatible_codec`, `invalid_remote_media` —
  carry the reason; `audio` is empty (no codec was negotiated) and the
  disconnect detail strings are blank. **CDR schema `version` → 2**: a
  strict consumer that exhaustively matched the v1 cause set won't
  recognise the new values, so the version is bumped per CLAUDE.md §7.7
  (the record shape is otherwise unchanged).

## [0.9.4] - 2026-06-18

### Added

- **DTLS-SRTP on the inbound delayed-offer offer (RFC 5763)** — the
  second DTLS-on-delayed follow-up; SiphonAI can now both answer (0.9.3)
  *and* offer DTLS on a delayed call. On an inbound delayed offer
  SiphonAI is the *offerer*, so with `[media].srtp` `preferred`/`required`
  and the new **`[media].srtp_offer = "dtls"`** it offers DTLS-SRTP in the
  200 OK (`UDP/TLS/RTP/SAVPF` + `a=fingerprint` + `a=setup:actpass`); the
  peer's answered fingerprint + setup arrive in the ACK, where SiphonAI
  derives its role (RFC 5763 §5) and enables the handshake. Surfaces on
  `start.srtp` (`exchange: "dtls"`). `[media].srtp_offer` defaults to
  `"sdes"` (the 0.9.2 behaviour); it only affects the delayed-offer path
  (inbound early offer always *answers* the peer's choice). SIPp
  `delayed_offer_dtls` phase added. **This completes SRTP for delayed
  offer** — SDES + DTLS, both directions. *Remaining delayed-offer
  follow-up: a per-call CDR for negotiations that fail before going
  active (today a metric + warn).*

## [0.9.3] - 2026-06-18

### Added

- **DTLS-SRTP on the outbound delayed-offer answer (RFC 5763)** — the
  first of the two DTLS-on-delayed follow-ups. When SiphonAI dials an
  **offerless** outbound INVITE and the peer's 2xx offers DTLS-SRTP
  (`UDP/TLS/RTP/SAVPF` + `a=fingerprint` + `a=setup:actpass`), SiphonAI
  now **answers** it: the gateway UAC's delayed-offer answer generator
  runs the inbound early-offer DTLS path (rewrite the offer for codec
  matching, patch the answer back to the SAVPF profile with our
  `a=fingerprint` + opposite `a=setup`, and `enable_dtls` as the
  handshake server). The generator gained a per-process DTLS certificate;
  the negotiated exchange (`dtls` vs `sdes`) is now carried on
  `OutboundAccepted` so `start.srtp.exchange` reports it correctly.
  Governed by `[[gateway]].srtp` like the SDES answer (0.9.1). SIPp
  `outbound_delayed_dtls` phase added. *DTLS on the **inbound**
  delayed-offer (where we'd offer DTLS) is the next follow-up.*

## [0.9.2] - 2026-06-18

### Added

- **SRTP on the inbound delayed-offer offer (SDES, RFC 4568)** — the
  mirror of the 0.9.1 outbound follow-up. On an inbound delayed offer
  SiphonAI is the *offerer*, so when `[media].srtp` (or a `[route.media]`
  override) is `preferred`/`required` the 200 OK now offers SDES
  (`RTP/SAVP` + `a=crypto`); the peer's answered key is installed from the
  ACK (`apply_answer`), and `required` fails the call if the peer answers
  plaintext. Surfaces on `start.srtp`. This reuses the existing
  `originate_offer`/`apply_answer` SDES path the delayed-offer accept
  already runs — it just stops hardcoding plaintext. SIPp
  `delayed_offer_srtp` phase added. *DTLS-SRTP on a delayed offer isn't
  produced (the SDES offer path only) — a remaining follow-up.*

## [0.9.1] - 2026-06-18

### Added

- **SRTP on the outbound delayed-offer answer (SDES, RFC 4568)** — the
  deferred 0.9.0 follow-up. An offerless outbound INVITE can't *offer*
  SRTP, but when the peer's 2xx carries an SDES offer (`RTP/SAVP` +
  `a=crypto`) SiphonAI now **answers** it: the gateway UAC's delayed-offer
  answer generator runs the same SDES negotiation the inbound early-offer
  path uses (rewrite the peer offer for codec matching, patch the answer
  back to `RTP/SAVP` with our `a=crypto`, install the keys on the leg).
  Governed by `[[gateway]].srtp` — `preferred` answers SRTP when offered
  (else plaintext), `required` fails the call on a plaintext peer offer.
  Surfaces on `start.srtp` and `siphon_ai_outbound_srtp_total{result}`
  like early-offer outbound SRTP. SIPp `outbound_delayed_srtp` phase
  added. *DTLS-SRTP on a delayed answer is not handled (no per-call cert
  in the generator), and SRTP on the **inbound** delayed-offer (where we
  offer) remains a separate follow-up.*

## [0.9.0] - 2026-06-18

Theme: **SIP delayed offer (offerless INVITE).** SiphonAI previously
**required** an inbound INVITE to carry an SDP offer and rejected an
offerless one, forcing interop partners (notably **Cisco CUCM**) to
insert a Media Termination Point. Delayed offer is now supported in both
directions, so the MTP can be removed and media flows directly. Protocol
stays `version: "1"` (no WS message change — `start` is just deferred by
one SIP round-trip until the codec is known from the answer); CDR stays
at its current `version` (the new outcomes surface as a metric, not a CDR
reason — a call that fails negotiation never went active).

### Added

- **SIP delayed offer (offerless INVITE), RFC 3264 — inbound and
  outbound.** Removes the forced **MTP** on CUCM (and similar)
  trunks/phones so media flows directly. Early-offer calls are unchanged.
  - **Inbound** (chunk 1): an inbound INVITE with no SDP is accepted —
    SiphonAI allocates media, puts **its own offer** in the 200 OK, and
    reads the peer's **answer from the ACK** before bridging. On by
    default; gate with `[sip].allow_delayed_offer = false` to force strict
    early offer (offerless INVITE then rejected `488`). The ACK-answer
    wait is bounded by SIP Timer H (~32 s); the call is active only after
    the answer is parsed. Metric `siphon_ai_delayed_offer_total{result}`.
  - **Outbound** (chunk 2): `POST /admin/v1/calls` with `"delayed_offer":
    true` dials an **offerless INVITE**, takes the peer's offer from the
    2xx, and answers in the **ACK** (via the gateway UAC's RFC-3264 answer
    generator). Delayed-outbound legs get transfer/hold like early-offer
    ones. (SRTP on the delayed-offer answer is a follow-up — the offerless
    INVITE can't carry an SDES offer.)
  - SIPp `delayed_offer` (inbound) and `outbound_delayed` phases added.

## [0.8.2] - 2026-06-17

### Added

- **Opus SDP `fmtp` (RFC 7587)** — the deferred 0.8.0 Opus follow-up.
  SiphonAI now advertises `a=fmtp:<pt> maxplaybackrate=16000;
  sprop-maxcapturerate=16000; stereo=0; sprop-stereo=0; useinbandfec=1;
  usedtx=0` for Opus — telling the peer we want **mono at ≤16 kHz** (the
  16 kHz bridge rate forge runs Opus at) and asking for in-band FEC. On
  the outbound **offer** it's keyed to our PT (111); on the **answer** it
  is re-keyed onto the *negotiated* payload type, so it survives a peer
  offering Opus at a different dynamic PT (the upstream negotiator carries
  fmtp forward by the offered PT, which would otherwise drop our tuning).
  Opus was already functionally correct without this — forge decodes mono
  at 16 kHz regardless — so these are quality/bandwidth hints, additive,
  no protocol/CDR change. Other codecs (G.711/G.722) remain fmtp-free.
  The SIPp `opus` phase now also `check_it`-asserts the answer `a=fmtp`.

## [0.8.1] - 2026-06-17

### Fixed

- **Outbound REGISTER advertised `0.0.0.0` in `Via` and `Contact` when
  `[sip].listen` used a wildcard bind** (`0.0.0.0:5060` / `[::]:5060`).
  The registration drive task used the socket *bind* address for the
  `Via` sent-by and `Contact` host, so a wildcard bind leaked into the
  outbound REGISTER — `0.0.0.0` is not a routable Contact, so registrars
  (e.g. CUCM) could not send INVITEs back, breaking inbound calls and
  registrar classification. REGISTER now advertises
  `[node].public_address` combined with the listen port — the same
  reachable address the inbound UAS already uses for its `Contact`. The
  socket still binds the configured (possibly wildcard) address; only
  the advertised SIP headers changed. A concrete, non-wildcard
  `[sip].listen` is unaffected. (`[node].public_address` is required
  whenever the bind is unspecified, so the advertised address is always
  routable.)

## [0.8.0] - 2026-06-17

Theme: **Opus codec support.** SiphonAI advertised only G.711/G.722 and
**rejected Opus at config load**; the v1 plan deferred it (DEV_PLAN §15.1)
as blocked on resampling. Opus is now negotiable. It runs at a **16 kHz
bridge rate** — `opus/48000/2` on the wire, but the WS path sees a 16 kHz
session (`start.audio.sample_rate = 16000`) — so the fixed 8/16 kHz PCM16
audio contract (CLAUDE.md §4.2) is unchanged and the WS protocol stays
`version: "1"`. **Off by default** (add `"opus"` to `[media].codecs`).
Minor-version bump because it adds SiphonAI's first **native build
dependency** (libopus). Delivered across three chunks (forge-media PR →
siphon-ai enablement → harness/docs/release).

### Added

- **Opus in `[media].codecs`.** A peer that offers `opus/48000/2` (or a
  route that lists `"opus"` for outbound) now negotiates Opus. The media
  engine (forge) runs the codec at 16 kHz mono — libopus decodes any
  encoded stream to 16 kHz and downmixes stereo→mono internally, and the
  encoder takes 16 kHz mono PCM (RFC 7587 — the RTP clock stays 48 kHz).
  RTP timestamps step at the 48 kHz clock; only the WS-facing PCM is
  16 kHz. The dynamic Opus payload type is preserved on the answer
  (RFC 3264). `docs/CONFIG.md`.
- **SIPp `opus` regression phase** (`opus_caller.xml`): offers Opus, asserts
  the 200 OK answers Opus and the daemon brings the call up at 16 kHz
  (`negotiated=opus sample_rate=16000`). Signalling only — the Opus
  encode/decode round-trip is forge unit-tested.

### Changed

- **Upstream forge-media pin `e95a31a959a6` → `3c82c2e5d175`** — adds the
  Opus 16 kHz bridge rate (forge-media#75, mirroring G.722's
  wire-clock-vs-PCM-rate split) and enables forge-engine's `opus` feature.
  Also picks up an unrelated SDES mid-call re-key API (forge-media#72),
  unused here.
- **New native build dependency: libopus** (via `audiopus`/`audiopus_sys`,
  built from source). Building `siphon-ai` now needs a C toolchain + CMake;
  the shipped Dockerfile already has them. `docs/DEPLOY.md` gains a build-
  prerequisites note. The runtime image is unaffected (statically linked).

### Notes

- **SDP `fmtp` (`stereo=0` / `useinbandfec` / `maxplaybackrate`) is a
  follow-up.** Opus is correct without it (the `/2` rtpmap is emitted and
  forge decodes mono regardless); the params interact with the answer's
  dynamic PT and want validation against a real softphone/carrier
  (`docs/design/DESIGN_OPUS.md` §7.5).

## [0.7.5] - 2026-06-17

Follow-up to 0.7.2: **bot-initiated hold on outbound legs.** The hold/resume
drive shipped in 0.7.2 was inbound-only — it built the hold/resume re-INVITE
offers from the inbound side's cached answer SDP, which the outbound originate
path didn't retain. This closes that gap, so a call placed via
`POST /admin/v1/calls` can be held/resumed by the WS server exactly like an
inbound call. No protocol or CDR change; hold remains always-available (no
flag).

### Changed

- **Outbound originated calls now support `hold` / `resume`.** `apply_answer`
  retains the SDP **offer** we sent (`OutboundAccepted.offer_sdp`), and the
  outbound `run_call` builds a `HoldContext` from it (direction-flipped to
  `sendonly` / `sendrecv`) with the same `DialogControl` it uses for outbound
  transfer (the directly-held dialog, re-INVITE via the gateway UAC). The gap
  music reuses the shared `[media].moh_file`.
- SIPp **outbound_bot_hold** regression phase (`outbound_bot_hold_uas.xml`):
  SiphonAI dials out, the echo-ws (`--auto-hold`) drives `hold`/`resume`, and
  the callee asserts it receives the sendonly/sendrecv re-INVITEs —
  `holds_total{result="ok"}` reads 2.

With this, both bot-hold and WS reconnect now work on inbound **and** outbound
legs.

## [0.7.4] - 2026-06-17

Follow-up to 0.7.3: **WS reconnect on outbound legs.** The reconnect drive
shipped in 0.7.3 was inbound-only — the controller logic is bridge-generic,
but the `[bridge].ws_reconnect_*` settings weren't threaded into the
outbound originate path. This closes that gap, so a call placed via
`POST /admin/v1/calls` reconnects the same way an inbound call does when
its WS drops. Still gated by `[bridge].ws_reconnect_enabled` (off by
default); no protocol or CDR change.

### Changed

- **Outbound originated calls now honour `[bridge].ws_reconnect_enabled`.**
  The originate path threads the daemon's reconnect settings (enabled,
  `ws_reconnect_max_secs`, and the shared `[media].moh_file` for the gap)
  through to the call controller and puts the leg's tap in survive-WS-drop
  mode — identical behaviour and code path to inbound. A new
  `OutboundService::with_moh_file` carries the hold-music file.
- SIPp **outbound_reconnect** regression phase: SiphonAI dials out, SIPp
  answers, the echo-ws (`--drop-after-ms`) drops, SiphonAI re-dials and
  resumes (`reconnected: true`), and the call ends cleanly — asserting
  `ws_reconnects_total{result="recovered"}`.

## [0.7.3] - 2026-06-17

Theme: **WS reconnect mid-call** — opt-in resilience. Until now, any
unexpected drop of the WebSocket to the developer's server killed the
call (fallback prompt → BYE → CDR `ws_disconnect`), so a server deploy /
restart / network blip took out every in-flight call. With
`[bridge].ws_reconnect_enabled = true`, SiphonAI instead keeps the SIP
call up on hold music and re-dials the **same** `ws_url`, resuming on a
fresh session keyed by the same `call_id` — falling back to teardown only
if no redial succeeds within a bounded window. **Off by default**; the WS
protocol stays `version: "1"` (additive) and the CDR schema stays at
version 1. Delivered across three chunks (config + protocol surface →
reconnect drive → observability/docs/harness/release).

### Added

- **Automatic WS reconnect (`[bridge].ws_reconnect_enabled`).** On an
  **unexpected** drop (server closed the socket without a `hangup`, an
  IO/TLS error, or a keepalive timeout) SiphonAI parks the caller on hold
  music and re-dials the same `ws_url` with exponential backoff
  (250 ms → ×2 → cap 5 s), resuming on a fresh session. Bounded by
  **`[bridge].ws_reconnect_max_secs`** (default 30) — how long the caller
  hears hold music before reconnect gives up and the §5.7 teardown runs.
  Both knobs take a per-route `[route.bridge]` override; enabling with a
  zero window fails loud at load. `docs/CONFIG.md`.
- **`start.reconnected`** — a new additive boolean on the `start` message
  (omitted-when-false, like `retrieved`), `true` on the session that
  resumes a call after a drop. The server should drop any handler it still
  holds for that `call_id` and treat the new socket as the live one; `seq`
  restarts at 0 and there is **no replay** of pre-drop audio/events.
  `docs/PROTOCOL.md` §3.1, §5.7.
- **Metric `siphon_ai_ws_reconnects_total{result=recovered|exhausted}`**
  and **CDR `reconnect { count, total_gap_ms }`** (additive, schema stays
  v1) — per-call reconnect-episode accounting. `docs/DEPLOY.md`.
- **SIPp `ws_reconnect` regression phase** — an echo-ws started with
  `--drop-after-ms` closes the socket mid-call; the daemon reconnects, the
  redial's `start` carries `reconnected: true`, and the call resumes and
  ends cleanly (asserts `ws_reconnects_total{result="recovered"}`).

### Changed

- **PROTOCOL.md §5.7 rewritten.** Reconnect is now supported (opt-in).
  With it on, **a call is ended by the `hangup` control message** — a bare
  WS socket close (even a clean `1000`) is treated as an unexpected drop
  and reconnected. With reconnect **off**, the v1 behaviour is unchanged.
- **`MediaTap` survive-WS-drop mode.** Internally, a reconnect-enabled
  call's tap treats a closed WS-facing channel as non-fatal (it holds for
  the redial) rather than tearing down — park parks *before* closing, but
  reconnect reacts *to* the close, so the tap had to learn to outlive it.

### Notes

- Inbound legs only this release. Outbound bot-hold and outbound reconnect
  remain follow-ups (the reconnect drive is bridge-generic, but the
  settings aren't threaded into the originate path yet).

## [0.7.2] - 2026-06-16

Theme: **bot-initiated hold/resume** — the WS server can now put its own
caller on hold and bring them back, with SiphonAI driving a true SIP
re-INVITE. Until now `hold`/`resume` existed only as inbound *events* (the
far end held *us*, §3.3); the bot could drive every other call-control
primitive (transfer, hangup, park, record, mute, DTMF, conference) but not
hold. This closes that gap. **No config flag** — hold is always available on
inbound legs; `[media].moh_file` only chooses what the held caller hears.
The WS protocol stays `version: "1"` (additive) and the CDR schema stays at
version 1. Delivered across three chunks (protocol surface → re-INVITE drive
→ observability/docs/SIPp/release).

### Added

- **`hold` / `resume` (server → SiphonAI).** The WS server puts *its own*
  caller on hold (`{ "type": "hold", "call_id": … }`) and resumes them
  (`resume`). SiphonAI becomes the re-INVITE **offerer** (`a=sendonly` to
  hold, `a=sendrecv` to resume), plays hold music to the caller, and stops
  forwarding caller audio to the server while held (no barge-in during
  hold). On success it replies `held` / `resumed` (§3.13) — past-tense acks,
  deliberately distinct from the §3.3 peer-hold events. `docs/PROTOCOL.md`
  §4.10, §3.13, §3.10.
- **`error { code: "hold_failed" }`.** A re-INVITE that's rejected, times
  out, can't resolve glare (RFC 3261 §14.1 — backoff + retry-once), or is
  refused because the far end already holds us (no hold-stacking in this
  first cut) fails the hold without dropping the call — it stays in its
  prior media state.
- **`[media].moh_file`.** Hold music for bot-initiated hold (shared shape
  with `[park].moh_file`): a WAV at the call's negotiated rate, validated to
  exist at load. Unset → generated comfort silence. `docs/CONFIG.md`.
- **CDR `hold { count, total_ms }`.** Per-call bot-hold accounting, mirroring
  `park`. Present only when the bot held the call at least once; omitted
  otherwise, so the CDR schema stays at version 1. Counts bot-initiated
  holds only — a far-end hold isn't tallied. `docs/DEPLOY.md`.
- **Metric `siphon_ai_holds_total{result=ok|failed}`.** Covers both
  directions (hold and resume). `docs/DEPLOY.md`.
- **SIPp `bot_hold` regression phase.** The inverse of
  `reinvite_hold_resume.xml`: an echo-ws started with `--auto-hold` drives
  `hold` → `resume` → `hangup`, and `bot_hold_caller.xml` asserts it
  *receives* a sendonly re-INVITE then a sendrecv one (SiphonAI is the
  offerer), with `siphon_ai_holds_total{result="ok"}` reading 2.
- **Playout-gated barge-in debounce (`[bridge.barge_in].debounce_ms`)**
  (#173 — merged between 0.7.1 and this release, so 0.7.2 is its first
  tagged release). While the bot is playing out, a `speech_started` is held
  for `debounce_ms` and only flushes if speech *sustains* past it — an
  echo / brief-background-noise gate that stops the bot cutting itself off
  on its own echo. Barge-in stays **immediate while the bot is silent**, so
  a caller interrupting between phrases is unaffected. `0`/unset = off
  (original immediate-flush behaviour); only affects `auto_clear`. Per-route
  override via `[route.bridge.barge_in].debounce_ms`. `docs/CONFIG.md`.

### Changed

- **Upstream siphon-rs pin `db45e42` → `8f3fd80`.** Adds
  `IntegratedUAC::send_reinvite_via_flow` — the flow-aware counterpart of
  `send_reinvite`, mirroring `send_refer_via_flow` over the INVITE
  transaction. Bot-hold needs it: on a TCP/TLS inbound dialog (e.g. Twilio
  TLS trunking) the peer's `Contact` names an ephemeral port nothing listens
  on, so the re-INVITE must reuse the inbound connection — the same fix
  `#157`/`#159` applied to BYE and REFER.
- **`TransferContext` refactored to embed a shared `DialogControl`**
  (`{ uac, source, flow }`), so hold and transfer share one dialog-resolution
  + connection-reuse path instead of duplicating it.

### Notes

- Inbound legs only this release. Outbound bot-hold needs the originated
  offer SDP plumbed through `apply_answer` to build the hold/resume offers;
  it's a follow-up.

## [0.7.1] - 2026-06-15

Theme: **outbound SRTP** — SiphonAI could *answer* an inbound SRTP offer but
only ever *offered* plaintext `RTP/AVP`, so outbound calls couldn't carry
audio on secure trunks (e.g. Twilio secure trunking). This closes that
inbound↔outbound asymmetry via SDES (RFC 4568) on the offer. **Off by
default**; the WS protocol stays `version: "1"` and the CDR schema is
unchanged. Self-contained in SiphonAI — no upstream forge-media change (the
crypto primitives are public at the pinned rev). Delivered across three
chunks (media-glue core → config/protocol/observability → SIPp/release).

### Added

- **Outbound SRTP via SDES (`[[gateway]].srtp`).** A call placed through a
  gateway with `srtp = "preferred" | "required"` now *offers* SRTP: SiphonAI
  mints an `AES_CM_128_HMAC_SHA1_80` master key, sends the INVITE as
  `RTP/SAVP` with an `a=crypto:` line, and on a 2xx that accepts it installs
  the send/recv keys onto the trunk leg (`session.srtp_a()` —
  `install_srtp_keys`), so the media is encrypted.
  * `[[gateway]].srtp` — `"off"` (default) | `"preferred"` | `"required"`,
    the outbound mirror of `[media].srtp`. `required` fails the call if the
    trunk answers plaintext; `preferred` continues unencrypted (downgrade).
    A per-gateway load-time warning fires when `srtp` is set but
    `transport != "tls"` (the SDES key would travel in cleartext on the
    signalling plane). `docs/CONFIG.md`, `docs/OUTBOUND.md`.
  * `start.srtp` (`{ exchange: "sdes", profile }`) is now populated on
    **outbound** calls too, the same shape inbound uses (this also corrects
    the stale "SDES not yet produced" note in `docs/PROTOCOL.md` — inbound
    SDES was already produced; only the outbound offer side was missing).
  * Metric `siphon_ai_outbound_srtp_total{result=encrypted|downgraded}`
    (`docs/DEPLOY.md`). A SIPp **outbound_srtp** regression phase exercises
    the full negotiation: a `required` gateway, SIPp answering `RTP/SAVP` +
    `a=crypto`, asserting the `encrypted` metric.
  * Implemented entirely in SiphonAI using public forge-sdp / forge-engine
    APIs at the current pin — no upstream PR, no pin bump.

## [0.7.0] - 2026-06-15

Theme: **conferencing + media-only call park** — two operator-controllable
multi-leg features, both **off by default** (fail-closed like `[outbound]`,
so a 0.6.x config upgrades with zero behaviour change). Conferencing mixes
N calls into one room where *every* leg keeps its own WS session (no single
"host" bot); call park shelves a call on hold music with **no** WS session,
to be retrieved later onto a fresh session by an operator. Delivered across
five chunks (room core → WS surface → conference admin → park → docs/SIPp/
release). The WS protocol version stays `"1"` — every addition is a new
message, event, or error code.

### Added

- **Conference admin CRUD (0.7.0 chunk 3 of 5).** Operators can compose and
  inspect rooms over the admin HTTP API; webhooks announce room lifecycle.
  All endpoints `501` when `[conference].enabled = false`. Same private-bind /
  no-native-auth posture as the originate API.
  * `GET /admin/v1/conferences` — list live rooms + their member call-ids.
  * `POST /admin/v1/conferences` — pre-create an (initially empty) room
    (`{room_id?, sample_rate?}`; `201 {room_id}`, generated id when omitted).
  * `DELETE /admin/v1/conferences/:id` — force-end a room; every member
    reverts to its direct pair (`conference_left { room_closed }`).
  * `POST /admin/v1/conferences/:id/participants` `{call_id}` — add **any**
    active call (inbound or outbound) to a room; `DELETE …/:call_id` removes
    one. Both `202` (dispatched): the daemon signals the target call, which
    joins/leaves on its own WS session — the outcome surfaces there
    (`conference_joined` / `conference_left` / `error`), not in the HTTP reply.
  * Cross-call add/remove respects CLAUDE.md §4.4 — it pushes a
    `ConferenceCommand` onto the target call's `CallHandle` (via a new
    daemon-wide bridge-id → handle `CallControlRegistry` populated by both the
    acceptor and the outbound service); that call's own controller runs the
    same join/leave path a WS `conference_join` would. No reaching into
    another call's state.
  * Webhooks `conference_created` (first join / pre-create) and
    `conference_ended { duration_ms, peak_participants }` (last leave /
    force-end), via a room-lifecycle observer. `docs/DEPLOY.md`, `docs/CONFIG.md`.

- **Conference-room core (0.7.0 chunk 1 of 5 — internal API only; the WS
  protocol + admin surfaces land in later chunks).** A room is one daemon
  task owning a `forge-mixer` `AudioMixer` and a 20 ms tick; joined calls
  contribute their SIP leg *and* their WS session as two mixer participants
  (DEV_PLAN_0.7.0.md §9.1), and every sink hears the room minus its own
  input — the caller never hears themselves, each bot still hears its own
  caller. Pieces:
  * `[conference]` config block (`enabled` — **off by default**, fail-closed
    like `[outbound]`; `max_rooms` 16; `max_participants_per_room` 8 calls;
    `join_tones`), validated at load. A 0.6.x config upgrades with zero
    behaviour change. `docs/CONFIG.md`.
  * `ConferenceRegistry` (core): exact-id `room_id → RoomHandle` map in the
    `CallRegistry`/`ConsultRegistry` §4.4 shape — rooms spawn on first join
    (locked to the first joiner's sample rate; mismatched joins rejected, no
    resampling in 0.7.0) and end on last leave.
  * Tap re-plumbing (`TapCommand::JoinRoom`/`LeaveRoom`): joining swaps the
    direct caller↔WS pair for room routing inside the tap task (single
    owner, no locks — the mute/flush pattern); leaving or the room dying
    always restores the direct pair. `clear`/`mute`/barge-in `auto_clear`
    also flush the bot's audio buffered in the room. Per-leg recording keeps
    working (right channel = the room mix the caller actually heard).
  * Mixing is drain-once + subtract-self: upstream's `mix_excluding` drains
    per call, so per-sink mix-minus-self is computed from one
    `get_all_participant_audio` pass per tick with upstream's own
    auto-gain/clamp semantics (a `mix_all_excluding` upstream API would
    replace this).
  * Metrics: `siphon_ai_conferences_active`,
    `siphon_ai_conference_participants`,
    `siphon_ai_conference_joins_total{result}`,
    `siphon_ai_room_tick_lag_seconds`,
    `siphon_ai_room_frames_dropped_total{stage,side}` (`docs/DEPLOY.md`).
  * New upstream deps: `forge-mixer`, `forge-injection` (same pinned rev as
    the rest of forge-media). Deliberately **not** `forge-conference` — its
    DTMF-IVR/PIN/host-control layer is out of scope per §9.4.

- **Conference WS protocol surface (0.7.0 chunk 2 of 5).** The WS server can
  now drive conferencing for its own call (self-scoped, §9.2); the protocol
  version stays `"1"` (all additions are new messages / a new error code).
  * Server → SiphonAI: `conference_join { room_id }` (creates the room if
    absent, subject to caps) and `conference_leave`. `docs/PROTOCOL.md` §4.8.
  * SiphonAI → server: `conference_joined { room_id, participants }`,
    `conference_left { room_id, reason }` (`reason` = `left` |
    `room_closed`), and the fan-out events `participant_joined` /
    `participant_left { room_id, participant_call_id }` to every *other*
    member when the room's composition changes. `docs/PROTOCOL.md` §3.12.
  * New `error` code `conference_failed` — a refused join (disabled, cap
    reached, sample-rate mismatch, already joined); the call continues on its
    direct pair.
  * Wired into both inbound (`BridgingAcceptor::with_conference`) and
    outbound (`OutboundService::with_conference`) calls; the daemon builds
    one shared `ConferenceRegistry` from `[conference]` when enabled. The
    async join runs off the controller's control loop (spawned, like REFER).
  * Reference echo server (`examples/echo-ws-server-python`) gains
    `--auto-conference-join ROOM` and logs the new events — the harness hook
    for the chunk-5 two-caller SIPp scenario.

- **Media-only call park + retrieve (0.7.0 chunk 4 of 5).** Park shelves a
  call **without** a WS session: the caller hears hold music, the SIP dialog
  + RTP stay up, and the call is later **retrieved** onto a *fresh* WS session
  (or times out / hangs up). The one chunk that reworks the per-call
  controller lifecycle — the media tap becomes the durable owner and the WS
  bridge becomes swappable. `docs/PARK.md`, `docs/design/DESIGN_0.7.0_PARK.md`.
  * `[park]` config block (`enabled` — **off by default**; `moh_file`
    optional, validated + decoded at load, comfort noise when unset or on a
    sample-rate mismatch; `timeout_secs` 300 / `0` = indefinite;
    `timeout_action` `hangup`|`keep`; `max_parked` 32). Global only.
    `docs/CONFIG.md`.
  * WS protocol (version stays `"1"`): `park { call_id, slot? }` (server parks
    its own call, self-scoped), `stop { reason: "park" }`, `start.retrieved`
    on a retrieved session, and `error` code `park_failed`. `docs/PROTOCOL.md`
    §3.1 / §3.9 / §3.10 / §4.9.
  * MOH on a 20 ms monotonic tick into forge playout (looping `FileSource` at
    the call's rate, else `forge-injection` comfort noise); a parked call's
    `MediaTap` task stays alive (it owns the forge media handle), while its WS
    bridge detaches and is re-spawned fresh on retrieve.
  * Admin API: `GET /admin/v1/parked`, `POST /admin/v1/calls/:id/park`
    `{slot?}`, `POST /admin/v1/calls/:id/retrieve` `{ws_url?}` (both `202`
    dispatched; retrieve is operator-only — there is no WS retrieve message).
    `501` when park is off, `404` unknown call, `409` retrieve of a non-parked
    call. `docs/DEPLOY.md`.
  * Observability: webhooks `call_parked` / `call_retrieved` / `park_timeout`;
    metrics `siphon_ai_parks_total{result}`,
    `siphon_ai_retrieves_total{result}`, `siphon_ai_parked_calls_active`; CDR
    `park { count, total_ms }` (additive, schema stays v1). Recording in
    progress at park keeps writing (records the MOH the caller hears).
  * Applies to inbound **and** outbound calls (any call in the
    `CallControlRegistry`). Reference echo server gains `--auto-park[=SLOT]`.

- **0.7.0 docs, SIPp coverage, and release (chunk 5 of 5).** Feature guides
  `docs/CONFERENCE.md` and `docs/PARK.md` (joining flow, admin control,
  limits, testing); doc-drift fixes in `CLAUDE.md` §8 and `docs/DEV_PLAN.md`
  (recording / outbound / conferencing / park are delivered, not "out of
  scope"; `forge-mixer` + `forge-injection` are now used). SIPp signaling
  regression gains three live phases — conference two-caller mix, park →
  retrieve → hangup, and park → timeout → hangup — each cross-checking the
  feature's metric.

## [0.6.2] - 2026-06-12

Theme: **TLS trunk hardening** — the fixes found by running v0.6.1 against a
production TLS trunk (Twilio secure trunking), plus the dispatcher growing
outbound TCP/TLS so gateways and registrations can dial secure trunks, not
just answer them. Everything new is off by default; the WS protocol stays at
`version: "1"` and the CDR schema is unchanged. A 0.6.1 deployment upgrades
with zero config changes.

### Added

- **Outbound dialing over TCP/TLS (`[[gateway]].transport`).** The transport
  dispatcher was inbound-only: any request needing a fresh TCP/TLS connection
  (an originated INVITE to a TLS trunk, a REGISTER to a TLS registrar) died
  with `outbound … without an existing stream is not supported in v1`. The
  dispatcher now owns client connection pools (`sip-transport`'s
  `ConnectionPool`/`TlsPool`, the pattern proven in siphond): outbound TCP/TLS
  with no established stream dials out through the pool, reuses the connection
  on subsequent requests, and the pool's reader feeds responses back into the
  same inbound packet pipeline the listeners use. TLS verifies the peer against
  the bundled webpki (Mozilla CA) roots — sufficient for public trunks like
  Twilio — plus an optional `[sip.tls_client].extra_ca` PEM bundle for
  private-CA deployments and self-signed test rigs (path validated at load).
  SNI is the gateway's proxy host, threaded through the existing
  `TransportContext::server_name`.
  * `[[gateway]]` gains `transport = "udp" | "tcp" | "tls"` (default udp).
    Non-UDP appends `;transport=…` to the Request-URI so RFC 3263 resolution
    selects the right transport; `tls` flips the default proxy port to 5061.
    With `register` set the transport is inherited from the register block and
    an explicit `transport` is rejected at load. `[[register]]` blocks with
    `transport = "tls"` — documented since 0.3.0 but broken by the same
    dispatcher gap — now actually go out over TLS.
  * Note: media on outbound legs is still plain RTP. Trunks that require SRTP
    (e.g. Twilio secure trunking) need the follow-up SDES change before
    outbound calls carry audio — this change is signaling-transport only.

- **Deepgram/LLM example bot: human-handoff transfer triggers**
  (`examples/deepgram-llm-bot-node/`). With `BOT_TRANSFER_TARGET` (a SIP URI)
  set, the bot hands the caller off via the protocol's `transfer` frame
  (PROTOCOL.md §4.4) through two routes sharing one announce-then-REFER path:
  a deterministic keyword fast-path over final utterances
  (`BOT_TRANSFER_PHRASE`, e.g. "transfer me" / "speak to a human"), and a
  `transfer_call` tool offered to the LLM so natural phrasings the regex
  misses still trigger the handoff. The tool only signals intent — the
  destination is always `BOT_TRANSFER_TARGET`; the model never chooses a URI.
  Example-only; no daemon changes.

### Fixed

- **TLS trunks: call transfer (REFER) failed with `transfer_failed` (#159).** The known
  gap left by the cleanup-BYE fix below: a `transfer` requested by the WS server on a
  call that arrived over TCP/TLS died with `send_refer: … transport error`, because
  upstream `send_refer` resolves the dialog's remote target and dials a fresh
  connection the inbound-only dispatcher refuses to open (and the peer's Contact names
  an ephemeral source port nothing listens on anyway). The transfer task now reuses the
  inbound connection captured at INVITE time: `TransferContext` carries the same
  `DialogFlow` that `TeardownContext` got in the BYE fix (attached in `run_call`, once
  the accepted session's transport is known), and `run_transfer_inner` sends both the
  REFER and the post-REFER BYE through the new upstream `send_refer_via_flow` /
  `bye_via_flow` (siphon-rs#58). `DialogFlow` additionally captures the receiving
  listener's local address so the auto-filled `Via` on flow-routed requests advertises
  the TLS listener's port instead of the UDP listener's (the cosmetic nit observed in
  the #157 verification). UDP dialogs and outbound (gateway-originated) legs keep the
  existing resolve-and-send path. Pin bumped to siphon-rs `db45e42251c3`, which also
  changes the `*_via_flow` call convention to the new `Flow` struct.

- **TLS trunks: daemon-initiated BYE never reached the peer (caller heard dead air
  after the bot hung up).** The companion to the Contact-port fix below, in the other
  direction. When the WS server ended the call (`hangup`), or a session timer / admin
  force-hangup drove teardown, the cleanup BYE was sent via `IntegratedUAC::bye`,
  which resolves the dialog's remote target and builds a fresh transport context —
  but the dispatcher is inbound-only and refuses to open a new TCP/TLS connection,
  so the BYE died with `outbound BYE failed … transport error` and the peer held the
  call until its own timeout. The acceptor now captures the inbound connection's
  writer channel at INVITE time (`DialogFlow`) and sends the cleanup BYE through
  `IntegratedUAC::bye_via_flow` over that same connection (RFC 5626 flow semantics).
  UDP dialogs keep the existing path. (The matching REFER gap this fix left open
  is also closed in this release — see the transfer entry above.)

- **TLS trunks: in-dialog ACK/BYE were lost (silent-tail recordings, wrong CDR cause).**
  When the daemon ran both a UDP and a TLS listener (`[sip].transports = ["udp", "tls"]`),
  the `Contact` on responses advertised the UDP listener's port with `transport=tls`
  (e.g. `<sip:siphon@<ip>:5060;transport=tls>`) regardless of which listener received
  the INVITE. A secure trunk (e.g. Twilio over TLS) honoured that Contact and dialed
  TLS to the UDP port, where nothing listens — so the caller's ACK and BYE never
  arrived and the call only ended when the RTP inactivity watchdog fired ~60 s later.
  Symptoms: call recordings padded with a ~60 s silent tail, CDR `cause = tap_ended`
  instead of a clean hangup, and `outbound BYE failed` warnings. Fixed upstream in
  siphon-rs (the auto-filled Contact port now follows the listener that received the
  request); this release threads the receiving listener's local address through the
  packet pump (`TransportContext::with_local_addr`) and bumps the siphon-rs pin.
  UDP-only deployments were never affected and their Contacts are unchanged.

- **SIPp suite portability to dual-stack hosts** (`test-harness/
  sipp-scenarios/run-all.sh`): sipp invocations now pin `-i 127.0.0.1`.
  Without it, sipp's `[local_ip]` can expand to `::1`, so UAS scenarios
  advertise an IPv6 Contact the IPv4-bound daemon can't reach — the
  in-dialog BYE fails with a transport error and the outbound /
  attended-transfer phases hang. The blind-transfer phase also gains
  the same venv-then-system-python3 fallback the other phases already
  had, instead of hard-requiring the CI-prepped venv. Harness-only;
  no daemon changes.

## [0.6.1] - 2026-06-10

Theme: **attended transfer** — the 0.6.0 fast-follow. The bot consults a
human before handing the caller off: SiphonAI places the consult leg as a
plain 0.6.0 outbound call (its own WS session), and completion is one
REFER-with-Replaces on the original call. The WS protocol stays at
`version: "1"` (one additive field) and the CDR schema is unchanged.

### Added

- **Attended transfer** — `transfer.replaces_call_id` names an answered
  outbound call (the consult leg, placed via `POST /admin/v1/calls` and
  identified by the `call_id` that endpoint returned). SiphonAI sends a
  REFER whose `Refer-To` embeds a `Replaces` built from the consult
  dialog, so the transferee connects directly to the consulted party
  (RFC 5589 §7). `target` becomes optional — the default Refer-To is the
  consult dialog's remote target (its 200 OK Contact); send `target` only
  to override the reachable URI. The consult leg is **not** torn down at
  REFER time (the transferee's INVITE-with-Replaces takes it over); to
  cancel a consultation, just hang up the consult call. Unknown / not-yet-
  answered / already-ended `replaces_call_id` → `error
  { code: "transfer_failed" }` and the call continues. `docs/PROTOCOL.md`
  §4.4.
- **Outbound legs are transferable** (blind or attended) — an outbound
  bot can hand its callee off the same way. The REFER goes out through
  the gateway's own UAC, so its digest credentials answer any 401/407
  challenge on the REFER.
- **Metric** — `siphon_ai_transfers_total{mode="blind"|"attended",
  result="accepted"|"rejected"|"local_error"}`; also back-fills blind
  transfers, which were previously unmetered.
- **SIPp coverage** — `attended_transfer_a.xml` + an always-on
  three-party regression phase (SIPp on both far ends: inbound transferee
  + consult callee; pass requires the REFER's `Refer-To` to carry
  `Replaces=` *and* the metric reading attended/accepted), driven by a
  new `--auto-transfer-replaces` test-harness knob on the echo WS
  example server.

### Fixed

- **Duplicate BYE after an accepted transfer** on inbound legs: the
  transfer task sends the post-REFER BYE ("REFER + BYE", RFC 5589 §6.1),
  but the acceptor's cleanup task then sent a *second* BYE from a fresh
  CSeq space — a protocol violation that strict peers reject. Affected
  blind transfer too (latent since 0.2.0; exposed by the new attended
  SIPp scenario's stricter tail).

## [0.6.0] - 2026-06-09

Theme: **outbound call origination.** SiphonAI inverts its inbound-only
model — `POST /admin/v1/calls` places a SIP call through a configured
gateway and bridges the answered call to a WS server over the same
protocol v1 session inbound calls use. **Off by default** (fail-closed on
`[outbound].max_concurrent = 0`) — a 0.5.0 deployment upgrades with zero
behaviour change. The WS protocol stays at `version: "1"` (the new
`start.direction` field is additive) and the CDR schema stays at version 1
(`direction` was reserved for outbound since v1).

### Added

- **Outbound origination** — `[outbound]` (`max_concurrent`,
  `rate_limit_per_sec`) + `[[gateway]]` blocks: standalone trunks
  (`proxy` / `from` / optional digest `auth_username` + `auth_password`)
  or `register = "<name>"` to dial through an existing `[[register]]`,
  inheriting its server, credentials, and AOR. Validated at config load.
  See `docs/OUTBOUND.md`.
- **Originate API** — `POST /admin/v1/calls` `{to, gateway, ws_url?,
  from?}` → `202 {call_id}`. **No built-in auth by design** (reverse-proxy
  posture, plan §9.5): bind the admin API private and front it yourself.
  The cap + rate limit are the native toll-fraud guardrails; the
  `503`/`429` rejections are fail-closed.
- **WS protocol** — `start.direction: "inbound" | "outbound"` (additive;
  servers that ignore it keep working). Outbound sessions start at answer
  and carry the dialed `to` and the caller-ID `from`.
- **Call-progress webhooks** — `outbound_initiated` `{to, gateway}`,
  `outbound_answered` `{sip_call_id}`, terminal `outbound_failed`
  `{cause}`; answered calls finish with the existing `call_end`. `cause`
  mirrors the metric's `result` labels.
- **CDR** — `direction: "outbound"` for answered originated calls;
  `route` carries the gateway name. Unanswered calls get no CDR (webhook +
  metric cover them), mirroring inbound where CDRs cover bridged calls.
- **Metrics** — `siphon_ai_outbound_calls_total{result="answered"|"busy"|
  "declined"|"no_answer"|"rejected"|"unreachable"|"failed"}` and the
  `siphon_ai_outbound_calls_active` gauge.
- **SIPp coverage** — `outbound_uas_answer.xml` + an always-on roles-
  inverted regression phase (SIPp answers SiphonAI's INVITE; pass requires
  the full INVITE → ACK → BYE flow *and* the answered-counter reading 1),
  driven by a new `--auto-hangup-after-ms` test-harness knob on the echo
  WS example server.
- **`docs/OUTBOUND.md`** — the outbound guide (enabling, originate API,
  the toll-fraud security posture, lifecycle, observability, testing
  without spending money, limitations).

### Notes

- Outbound calls **spend money**. The security model is deliberate:
  no native API auth, so the documented posture (private bind +
  authenticating reverse proxy + `max_concurrent` + `rate_limit_per_sec`
  + trunk-side destination allowlists) is mandatory reading —
  `docs/OUTBOUND.md` §3.
- Not in 0.6.0: early media (WS session starts at answer), attended
  transfer (the 0.6.1 fast-follow), outbound recording, outbound
  STIR/SHAKEN signing, built-in AMD (the WS server's job, by design).

## [0.5.0] - 2026-06-08

Theme: **call recording.** Each call's audio can be captured to a stereo WAV
(caller on the left channel, bot/WS on the right) for compliance and QA.
**Off by default** — a 0.4.x deployment upgrades with zero behaviour change
until `[recording].mode` is set. The WS protocol stays at `version: "1"`
(the new recording messages are additive) and the CDR schema stays at
version 1 (the new fields are additive optionals).

### Added

- **Call recording** (`[recording]`) — writes `<dir>/<call_id>.wav`, stereo
  PCM16 at the call's negotiated rate. `mode = "off"` (default) / `"always"`
  (whole call) / `"on_demand"` (WS-server-driven). The recorder runs off the
  audio hot path (CLAUDE.md §4.3): the media tap only does a non-blocking
  copy onto a bounded channel, and a per-call writer task does the file I/O —
  a backed-up writer drops frames (flagged `degraded`) rather than ever
  stalling or gapping the live call. See `docs/RECORDING.md`.
- **Per-route override** — `[route.recording].mode` strictly overrides the
  global mode for matched calls (mirrors `[route.security]`). The output
  `dir` is the global one, so `[recording].dir` is required (and created at
  load) whenever any route enables recording, even with the global mode
  `off`.
- **On-demand control (WS protocol).** New `BridgeIn`: `start_recording` /
  `stop_recording` / `pause_recording` / `resume_recording`. New
  `BridgeOut`: `recording_started` / `recording_stopped` /
  `recording_failed` (each with `recording_id`). `pause_recording` **omits**
  the paused span from the file (dropped, not silenced) — the PCI
  "pause while the caller reads a card number" primitive. PROTOCOL.md §3.11 /
  §4.7.
- **CDR pointer** — `recording_id` / `recording_path` on the CDR (additive
  optionals, omitted when the call wasn't recorded → schema stays at v1).
- **Metric** — `siphon_ai_recordings_total{result="ok"|"degraded"|"failed"}`.
- **`docs/RECORDING.md`** — the recording guide (enabling, output format,
  on-demand control, observability, the hot-path/degraded story, disk
  sizing, retention, consent, and limitations), plus an always-on recording
  phase in the SIPp regression suite that asserts a valid stereo WAV.

### Notes

- Recordings are written **decrypted** — even for SRTP-encrypted calls, the
  WAV on disk is plaintext PCM (forge decrypts the media to bridge it; the
  recorder taps the decoded audio). The recording directory is sensitive
  data; protect it at rest (disk encryption, permissions) and manage
  retention yourself — the daemon never deletes recordings. Consent and any
  "this call is recorded" announcement are the operator's responsibility.
- **SRTP re-key on a timer** was planned to ride along but was **deferred**:
  forge-media has no coordinated mid-call re-key (DTLS renegotiation is
  blocked; a unilateral key swap would break media), so it needs upstream
  work first. See `docs/design/DEV_PLAN_0.5.0.md` §3.2 / §6.

## [0.4.1] - 2026-06-07

Completes the 0.4.0 STIR/SHAKEN theme — the four items deferred from that
release, plus the small feature that makes the passing path testable. Still
**off by default**; protocol stays at `version: "1"` (the one new `verstat`
field is additive).

### Added

- **PASSporT `iat` freshness check (replay protection).** With verification
  enabled, a PASSporT whose `iat` is outside `[security.stir_shaken]
  .iat_freshness_secs` of now (past **or** future), or missing, now fails —
  surfaced as the new `verstat.iat_passed` boolean and folded into the
  composite pass. Default window 60 s (ATIS-1000074); `0` disables the check
  for upstreams with broken clocks.
- **`[security.stir_shaken].x5u_tls_extra_ca`** — optional supplemental CA
  bundle trusted **for the `x5u` HTTPS fetch only** (added to the public
  web-PKI roots), for operators hosting `x5u` behind a private/lab CA.
  Validated at load when enabled. Does not affect the SHAKEN chain, which
  always validates against `trust_anchors`.
- **`docs/SECURITY_STIR_SHAKEN.md`** — the STIR/SHAKEN security model:
  attestation is a signal not a verdict, the two trust domains, the
  `verstat` trust rule, replay/freshness, observe-first gate rollout,
  monitoring, and limitations.
- **Twilio Caller Identity cross-check recipe** — a `docs/INTEGRATIONS_TWILIO.md`
  section and a runnable `examples/verstat-compare-py` server that compares
  SiphonAI's independent `verstat` against Twilio's `X-Twilio-VerStat`
  header (forwarded via `[bridge].forward_headers`), logging AGREE/DIVERGE.
- **Passing-attestation SIPp regression** (`stir_shaken_attestation_pass.xml`)
  plus the `gen_test_passport` example (a `siphon-ai-stir-shaken` example
  that mints a CA + leaf + x5u TLS cert + fresh signed PASSporT, doubling as
  an operator lab tool). The first *green* verstat path under CI — a
  fully-verifiable call is admitted, alongside the 0.4.0 428/403 rejects.

### Changed

- **`verstat.iat_passed` is part of the composite `passed()`** — a
  deployment that already opted into `stir_shaken` will now reject a
  previously-passing call that carries a stale `iat`. This is the
  spec-correct outcome; tune or disable it via `iat_freshness_secs`.

## [0.4.0] - 2026-06-07

Theme: **STIR/SHAKEN call authentication.** Inbound INVITEs carrying an
RFC 8224 `Identity` header are now verified end-to-end — PASSporT decode
(RFC 8225), ES256 signature, X.509 chain validation to a configured STI-PA
trust anchor (via the `x5u` certificate, fetched and TTL-cached), and the
SHAKEN `orig`/`dest` ↔ SIP `From`/`To` claim checks — yielding a per-call
*verstat* verdict. Operators can gate on it (`min_attestation` 4xx,
`require_identity` 428, with per-route overrides), and the verdict is
surfaced everywhere observability already reaches: the WS `start` message,
the CDR, a structured log line, and a new HEP3 chunk for Homer.

Everything is **off by default** — a 0.3.x deployment upgrades with zero
behaviour change until `[security.stir_shaken].enabled = true`. Protocol
stays at `version: "1"`: `start.verstat` is an additive optional field, so
v1 WS servers built against earlier releases keep working unchanged. The
cryptographic core lives in two new building blocks — siphon-rs's
`sip-identity` crate (parsing + ES256 + chain validation) and this repo's
`siphon-ai-stir-shaken` crate (the `x5u` fetch, cert cache, and verdict
orchestration the stack crate deliberately leaves to the application).

### Added

- **`siphon-ai-security` crate — the verstat vocabulary.** `AttestationLevel`
  (SHAKEN A/B/C with an explicit trust rank), `VerificationResult` (the
  verdict, with a `trusted_attestation()` accessor that only trusts a claim
  when verification fully passed), and the `MinAttestation` policy gate
  (strict per-route `resolve` + the §4 `permits` matrix). Dependency-light
  so every layer can depend on it cheaply.

- **`[security]` / `[security.stir_shaken]` config surface.** `enabled`,
  `trust_anchors` (PEM bundle path, validated at load), `cert_cache_ttl_secs`
  (default 1 h), `require_identity`, plus the gate knobs `min_attestation`
  (`none`/`A`/`B`/`C`) and `min_attestation_response` (403/488/606). Fully
  inert by default; misconfiguration fails loud at startup. See
  [`docs/CONFIG.md`](docs/CONFIG.md).

- **`siphon-ai-stir-shaken` crate — the verifier service.** The
  application-layer half of verification: HTTPS `x5u` certificate fetch
  (https-only, redirect-free, size/time-capped), a process-wide TTL cert
  cache keyed by URL, trust-anchor loading, and verdict orchestration
  (`Verifier::verify(identity, from, to) → VerificationResult`). The
  cryptographic core (ES256 + X.509 chain validation) is siphon-rs
  `sip-identity`; this crate wires it to the network and the cache.

- **Accept-path verification + the verstat surface.** Each inbound INVITE
  is verified before route/media bring-up; the verdict rides
  `BridgeOut::Start` as the optional `verstat` object and lands on the CDR
  as `verstat_attest` / `verstat_passed` (additive — CDR schema stays at
  version 1; emitted only when verification is enabled). One `info!` line
  per call carries the verstat fields. See [`docs/PROTOCOL.md`](docs/PROTOCOL.md).

- **Attestation policy gate.** After verification, before route matching,
  the daemon can reject calls that don't meet policy — `require_identity`
  → **428 Use Identity Header** (RFC 8224 §6.2.2) for an INVITE with no
  `Identity` header, and a `min_attestation` floor → the configured
  **403/488/606** with a `Reason: Q.850;cause=21` header. The gate runs
  before media is allocated, so a rejected call never opens an RTP port or
  WS bridge. Per-route override via `[route.security].min_attestation`
  (strict override, like `[route.media].srtp`). See
  [`docs/CONFIG.md`](docs/CONFIG.md) and [`docs/DIALPLAN.md`](docs/DIALPLAN.md).

- **HEP3 verstat chunk for Homer.** When HEP is enabled, the verdict ships
  as a `HepProtocol::Verstat` (chunk type `0x66`) packet correlated by SIP
  `Call-ID`, threading onto the same call view as the SIP / RTCP / CDR
  chunks. JSON payload, same shape as `start.verstat`. Requires the
  upstream `hep-rs` `Verstat = 102` protocol type
  ([thevoiceguy/hep-rs#1](https://github.com/thevoiceguy/hep-rs/pull/1)).
  See [`docs/HEP.md`](docs/HEP.md).

- **New metric `siphon_ai_verstat_total{result=passed|failed|unsigned}`** and
  a `rejected_attestation` label on `siphon_ai_invites_total` so
  STIR/SHAKEN policy rejections are separately alertable from ordinary
  routing/media rejects. See [`docs/DEPLOY.md`](docs/DEPLOY.md).

- **`contrib/sti-pa-roots.pem` trust-anchor template + `contrib/README.md`.**
  A documented placeholder (not a baked-in root — a stale or wrong trust
  anchor is a security defect): the operator populates it with the
  authentic STI-PA root(s), verified by fingerprint. Using it unpopulated
  fails loud at startup by design.

- **STIR/SHAKEN SIPp regressions.** `stir_shaken_no_identity_428.xml` and
  `stir_shaken_attestation_403.xml` exercise the accept-path gate end-to-end
  over real SIP (both reject before media), run in a new always-on
  `stir_shaken` phase of the regression suite.

### Changed

- **`siphon_ai_rtp_rtt_ms` now renders as a bucketed Prometheus histogram instead of a summary.** The metric had no explicit buckets registered, so `metrics-exporter-prometheus` fell back to a summary (quantiles) — inconsistent with the other `_seconds` histograms and awkward to aggregate across instances. It now has explicit ms buckets (10 ms–1 s) via `set_buckets_for_metric`, matching the 0.3.0 housekeeping rule ("histograms get sensible buckets defined explicitly"). `/metrics` now emits `siphon_ai_rtp_rtt_ms_bucket{le="…"}` lines; anything scraping the old `{quantile="…"}` series should switch to `histogram_quantile()` over the buckets.

## [0.3.2] - 2026-06-05

Closes the last open 0.3.0 Definition-of-Done item: `rtcp_rtt_ms` now
populates on live calls.

### Fixed

- **`rtp_stats.rtcp_rtt_ms` is now populated instead of always `null`** — picked up via a forge-media bump (`5c30c03e17f4` → `e95a31a959a6`, [thevoiceguy/forge-media#69](https://github.com/thevoiceguy/forge-media/pull/69)). The `rtcp_rtt_ms` field has shipped since 0.3.0 but always emitted `null`: forge-engine's terminator mode generates an RTP stream toward the carrier (its own SSRC) yet never originated RTCP **Sender Reports** for it, so the carrier's Receiver Reports came back with `last_sr = 0` and the `RttTracker` (RFC 3550 §A.7) had nothing to match against. 0.3.0 plan §9 decision 10 deferred the SR emitter as a follow-up; this is it. forge-engine now sends an SR per generated stream every 5 s (RFC 3550 §6.2 minimum), SRTCP-protected, and resolves the echoed `last_sr`/`delay_since_last_sr` from incoming RRs into the RTT sample carried on `RtcpReportReceived`. SiphonAI already consumed the field (`media-glue` populates `rtcp_rtt_ms` on the `rtp_stats` WS event and records the `siphon_ai_rtp_rtt_ms` histogram), so no SiphonAI-side code change — the value simply starts flowing. Expect a sample on each RR (~every 5 s) once both directions of RTCP are live.

## [0.3.1] - 2026-06-05

Carrier-interop hardening for the 0.3.0 encryption stack. 0.3.0 shipped
TLS, mTLS, and DTLS-SRTP, but its SRTP coverage was self-paired — so a
cluster of spec-conformance bugs stayed invisible until a spec-correct
carrier (Twilio Secure Trunking) was on the wire: AES-CM IV byte offsets,
SRTCP KDF labels, RTCP SR/RR report-count parsing, and an always-set RTP
marker bit — all fixed here via forge-media bumps. It also brings forward
the 0.3.0 §6 carry-forward — SDES SRTP outbound (`RTP/SAVP`) — to unblock
carriers whose all-or-nothing "Secure Trunking" toggle mandates TLS
signaling and SRTP together. Rounded out with RFC 3261 §12 / §13 / §20
response polish and journald/observability fixes.

Note: 0.3.0 was prepared (version bump + changelog) but never tagged; its
encryption features ship to users for the first time here, hardened.

Protocol stays at `version: "1"` — every addition is additive, so v1 WS
servers built against 0.1.0 / 0.2.0 keep working unchanged.

### Fixed

- **SRTP audio now decrypts cleanly against spec-correct peers** — picked up via a forge-media bump (`48ff87be0a85` → `33443589ce2e`, [thevoiceguy/forge-media#67](https://github.com/thevoiceguy/forge-media/pull/67)). The four AES-CM IV construction sites in `forge-rtp` placed the packet index in the wrong bytes of the 128-bit IV (RTP 48-bit index at `iv[6..12]` instead of `iv[8..14]`; SRTCP 32-bit index at `iv[8..12]` instead of `iv[10..14]`, both per RFC 3711 §4.1.1 / §4.1.2). Symmetric protect/unprotect round-trip tests passed because both ends used the same wrong offsets and AES-CTR cancelled — bug stayed invisible until a spec-correct peer (Twilio Secure Trunking) was on the wire. Concrete production symptom on the first SDES SRTP Twilio call: caller heard white noise instead of the bot's greeting (our outbound was unrecoverable garbage to Twilio), and the bot's STT received PCMU-shaped bytes that didn't decode to recognisable speech (Twilio's inbound was unrecoverable garbage to us, so no LLM turn ever fired). DTLS-SRTP runs through the same code path; existing DTLS callers were silently affected the same way against any spec-correct peer — the 0.3.0 DTLS-SRTP coverage was self-paired and didn't surface it. No SiphonAI-side code change.

- **SRTCP packets from spec-correct peers now authenticate successfully** — picked up via a forge-media bump (`f599ebd6cd39` → `48ff87be0a85`, [thevoiceguy/forge-media#66](https://github.com/thevoiceguy/forge-media/pull/66)). `forge-rtp`'s `derive_session_keys` always derived with the SRTP labels (`0x00` / `0x01` / `0x02`) regardless of which protocol was calling it; SRTCP requires labels `0x03` / `0x04` / `0x05` per RFC 3711 §4.3.3. Result was that every SRTCP packet from Twilio / FreeSWITCH / any spec-correct peer was discarded with "SRTCP authentication failed" — visible in the journal every ~5 s (the RTCP send interval). Surfaced immediately once SDES SRTP shipped on the siphon-ai side and real carrier RTCP started landing; DTLS-SRTP 0.3.0 coverage was hand-driven and audio-focused, so SRTCP didn't get exercised end-to-end. SRTP path is unchanged. No SiphonAI-side code change.

- **Outbound RTP no longer sets the marker bit on every packet** — picked up via a forge-media bump (`33443589ce2e` → `5c30c03e17f4`, [thevoiceguy/forge-media#68](https://github.com/thevoiceguy/forge-media/pull/68)). `forge-engine`'s playout scheduler set the RTP marker on the first frame of each *append call*, but SiphonAI streams one 20 ms frame per call — so every outbound packet carried `M=1` instead of only the first packet of each talkspurt (RFC 3551 §4.1). Confirmed against Twilio Secure Trunking: 100 % of outbound packets were marked, while Twilio's inbound correctly marked only talkspurt starts. Not audible (the static was the separate AES-CM IV bug above), but an interop wart — stricter jitter buffers can treat every marked packet as a fresh talkspurt and needlessly re-adjust playout. The fix keys the marker off a persistent wall-clock talkspurt detector (audio resuming after a >60 ms silence gap, or a barge-in `Replace`); verified on the wire post-deploy as 2 of 317 outbound packets marked, both at talkspurt starts. No SiphonAI-side code change.

### Added

- **SDES SRTP outbound — inbound `RTP/SAVP` offers now negotiate end-to-end** (the 0.3.0 plan §6 carry-forward, brought forward to unblock production deployments where the carrier ships an all-or-nothing "Secure Trunking" toggle that requires TLS signaling AND SRTP — most notably Twilio Elastic SIP Trunk). When `[media].srtp = "preferred"` or `"required"` and the offer's audio m-line is `RTP/SAVP` (or `RTP/SAVPF` without TLS), the daemon now:
  1. Parses the offer's `a=crypto:` attributes via `forge_sdp::sdes`,
  2. Selects the strongest mutually-supported crypto suite (default preference `AES_CM_128_HMAC_SHA1_80`),
  3. Calls `forge_sdp::sdes::answer_sdes()` to derive the inbound and outbound SRTP master keys plus a freshly-generated local `a=crypto:` line,
  4. Patches the SDP answer with `RTP/SAVP` profile + the local crypto attribute,
  5. Installs the derived keys into the per-call `SrtpContext` via the new `forge_engine::srtp_install::install_srtp_keys` (forge-media PR #65), at which point the ordinary `protect_rtp` / `unprotect_rtp` path takes over.

  `start.srtp` on the WS protocol populates as `{exchange: "sdes", profile: "AES_CM_128_HMAC_SHA1_80"}` so the bridge server knows the call is SDES-protected (distinct from the existing `exchange: "dtls"` value for the DTLS-SRTP path).

  Policy matrix is now complete:

  | Mode | Plain RTP | DTLS-SRTP | SDES |
  |---|---|---|---|
  | `off` | ✓ | 488 | 488 |
  | `preferred` | ✓ | ✓ | ✓ |
  | `required` | 488 | ✓ | ✓ |

  Malformed SDES offers (no `a=crypto:`, no acceptable crypto suite, malformed inline key) surface as 488 the same way DTLS-SRTP fingerprint-mismatches do — no silent downgrade. Seven new unit tests cover the negotiation, profile rewrite, missing-crypto rejection, and SAVP-vs-SAVPF disambiguation against the existing DTLS-SRTP helper.

### Fixed

- **Log output no longer emits ANSI colour escape sequences when stdout isn't a terminal.** `bins/siphon-ai/src/main.rs` builds the tracing subscriber from a hand-composed `fmt::layer()` rather than the higher-level `fmt::Subscriber::builder()` (to get a reload handle for the EnvFilter). The layer form defaults to ANSI on regardless of tty status — so every line under systemd was landing in journald with embedded `\x1b[..m` sequences. Harmless to human readers (journalctl strips them on display), but it silently broke every downstream consumer that does string matching against the journal — most notably the fail2ban `<HOST>` extractor in our trunk-rejection filter, which saw `peer=\x1b[3m185.9.19.90:61792\x1b[0m` and never matched. The fmt layer now calls `.with_ansi(std::io::IsTerminal::is_terminal(&std::io::stdout()))` so ANSI is enabled for interactive `cargo run` but disabled under systemd. After upgrading, restart fail2ban (`sudo systemctl restart fail2ban`) so its journal cursor advances past the polluted entries; subsequent 403s will match the filter.

- **RTP QoS metrics are no longer garbage for any real SIP peer** — picked up via a forge-media bump (`f7cd7f074d7c` → `47cf68aa0f0a`, [thevoiceguy/forge-media#63](https://github.com/thevoiceguy/forge-media/pull/63)). `forge-rtp`'s SR/RR parsers were ignoring the `RC` field in the RTCP common header and greedily consuming 24-byte chunks until the input buffer ran out — treating the trailing SDES bytes of every compound RTCP packet (which RFC 3550 §6.1 makes mandatory) as phantom reception report blocks. The wrong bytes landed in `jitter`, `cumulative_lost`, `last_sr`, etc., silently corrupting every downstream metric and `RtpStats` WS event. Observed pre-fix: `siphon_ai_rtp_jitter_ms` averaged ~113 M ms per RR against a real Twilio inbound (formula was decoding ASCII SDES CNAME bytes as the jitter field). Post-fix: `jitter_ms` / `packet_loss_ratio` / `rtt_ms` reflect actual call quality; the `rtp_stats` WS events your bot can use for adaptive logic are now trustworthy. No SiphonAI-side code change; the fix is entirely in `forge-rtp::rtcp::{SenderReport,ReceiverReport}::parse`, which now take an explicit `report_count: u8` argument wired through from the RTCP header.

- **Responses now emit `Server:` instead of `User-Agent:`, advertise `Allow:` on 2xx to INVITE, and omit empty `Supported:` on OPTIONS 200 OK** — picked up via a siphon-rs bump (`47cd5d39c7d6` → `a4f8521561d6`, [thevoiceguy/siphon-rs#52](https://github.com/thevoiceguy/siphon-rs/pull/52)). Three independent RFC 3261 §13/§20 polish items: (1) §20.41 / §20.50 — responses identify the UAS via `Server:`, requests use `User-Agent:` (we were emitting the latter on responses; carriers tolerated it but it confused header-name-strict SIP analysers); (2) §13.2.1 — 2xx to INVITE SHOULD advertise the methods the UAS supports so the peer learns what mid-dialog requests (re-INVITE / UPDATE / REFER / INFO) are legal without an OPTIONS probe; (3) §20.37 — an empty `Supported:` value implies nothing useful and some peers treat the blank as a parse oddity. No SiphonAI-side code change.

- **`200 OK` to INVITE now carries the request's `Record-Route` headers** — picked up via a siphon-rs bump (`d0d3691244de` → `47cd5d39c7d6`, [thevoiceguy/siphon-rs#51](https://github.com/thevoiceguy/siphon-rs/pull/51)). The UAS response builder previously dropped every `Record-Route` from the INVITE, in violation of RFC 3261 §12.1.1. Subsequent in-dialog requests (ACK / BYE / re-INVITE / REFER) routed straight to the UAS's `Contact` instead of traversing the proxy chain — silent under carriers like Twilio (whose edge tolerates direct-to-Contact in-dialog routing), but a latent dialog-killer behind stricter SBCs or multi-proxy topologies. No SiphonAI-side code change; the fix is entirely in the upstream UAS builder.

## [0.3.0] - 2026-05-26

Third release. Theme: **trust and encryption** — every transport
the daemon touches can now run encrypted. SIP/TLS gets hot cert
reload (no in-flight call drops on renewal). The WebSocket bridge
gets mTLS with optional SPKI cert pinning. Inbound calls offering
DTLS-SRTP get a SAVPF answer end-to-end (forge handles the
handshake, derives SRTP keys, decrypts media). RTP-quality events
(`jitter_ms`, `packet_loss_ratio`, and an `rtcp_rtt_ms` field
reserved for 0.3.1) now actually populate.

Protocol stays at `version: "1"` — every new variant is additive,
so v1 WS servers built against 0.1.0 / 0.2.0 keep working
unchanged. The wire-shape additions land *behind* the new config
defaults: out of the box, 0.3.0 behaves like 0.2.0.

### Added

#### Encryption

- **DTLS-SRTP for inbound calls** (PROTOCOL §3.1 `start.srtp`,
  DEV_PLAN_0.3.0.md §4.1). When the offer's audio m-line is
  `UDP/TLS/RTP/SAVPF` and `[media].srtp` is `"preferred"` or
  `"required"`, the daemon:
  1. extracts the remote `a=fingerprint:` + `a=setup:` from the
     offer,
  2. answers `UDP/TLS/RTP/SAVPF` with our own SHA-256 fingerprint
     and `a=setup:passive` (RFC 5763 §5),
  3. provisions the DTLS leg on the per-call `MediaSession`,
     forge-engine's recv loop demuxes the inbound DTLS handshake
     (RFC 5764 §5.1.2 first-byte demux),
  4. on handshake completion, the derived SRTP master keys
     install into the existing `SrtpContext` and subsequent SRTP
     packets decode through the ordinary unprotect path.

  `start.srtp` is populated with `{exchange: "dtls", profile:
  "AES_CM_128_HMAC_SHA1_80"}` — the profile is best-guess
  pre-handshake (RFC 5764 mandates that suite as baseline; the
  actual negotiation may select a stronger AES-GCM suite).

  Long-lived per-process DTLS cert generated at daemon startup
  (rcgen). Same cert presented to every DTLS handshake; rotation
  is via daemon restart (or `systemctl reload` on the SIP/TLS
  side — DTLS-SRTP cert rotation is intentionally NOT exposed,
  since rotating it mid-call would invalidate in-flight handshakes).

  SDES (`RTP/SAVP` / `RTP/SAVPF`) offers are rejected with 488 —
  forge-sdp ships the `a=crypto:` parser but the forge-engine
  producer wiring isn't done. 0.3.1.

- **`[media].srtp` config + policy gate**. New
  `[media].srtp = "off" | "preferred" | "required"` (default
  `"off"`, matches 0.2.0). Per-route override via
  `[route.media].srtp`. The policy matrix is enforced before any
  media bring-up — incompatible offers fail fast with 488:

  | Mode | Plaintext (`RTP/AVP`) | DTLS-SRTP | SDES |
  |---|---|---|---|
  | `off` | ✓ | 488 | 488 |
  | `preferred` | ✓ | ✓ | 488 |
  | `required` | 488 | ✓ | 488 |

  Resolution via `resolve_srtp_mode(defaults, route)` mirrors the
  other `resolve_*` helpers; unknown route-level values warn and
  fall back to defaults.

- **mTLS for the bridge WebSocket leg** (`[bridge.tls]` block,
  DEV_PLAN_0.3.0.md §4.2 Part A, `docs/DEPLOY.md` §3a). New
  config:

  ```toml
  [bridge.tls]
  client_cert    = "/etc/siphon-ai/bridge/client.pem"
  client_key     = "/etc/siphon-ai/bridge/client.key"
  pinned_sha256  = "..."   # optional 64-hex-char SPKI SHA-256
  ```

  Builds a custom `rustls::ClientConfig` and hands it to
  `tokio-tungstenite`'s `Connector::Rustls`. The optional SPKI
  pin (SHA-256 of the server's `SubjectPublicKeyInfo` per
  RFC 7469 §3) replaces default CA verification with exact-match,
  appropriate for carrier-pinned PBX deployments. Cert / key /
  pin validation happens at config compile so issues surface at
  daemon startup, not at first call.

- **Outbound TLS UAC for REGISTER** (DEV_PLAN_0.3.0.md §4.5,
  `docs/REGISTRATION.md` "TLS registration"). `transport = "tls"`
  on a `[[register]]` block now actually goes out over TLS — no
  silent fallback to UDP. Uses the daemon-wide webpki trust
  store (Mozilla CA bundle). Twilio Elastic SIP Trunk recipe in
  `REGISTRATION.md`. The stale "Inbound UAS only" disclaimer in
  `CONFIG.md` is removed.

- **SIGHUP hot cert reload for SIP/TLS** (DEV_PLAN_0.3.0.md
  §4.3). `systemctl reload siphon-ai` rotates `[sip.tls].cert` +
  `.key` without dropping in-flight TLS sessions. In-flight
  dialogs keep using the cert they handshook with
  (RFC 5746-compliant); new connections pick up the fresh cert.
  Broken PEM on reload doesn't kill the daemon — `error!`
  logged, previous cert keeps serving. New metric
  `siphon_ai_sip_tls_reload_attempts_total{outcome}`. systemd
  `ExecReload=/bin/kill -HUP $MAINPID`. Builds on siphon-rs's
  `run_tls_with_swappable_config` (#49).

#### Observability

- **`rtp_stats` event fields populate** (PROTOCOL §3.8,
  DEV_PLAN_0.3.0.md §4.4). `jitter_ms` and `packet_loss_ratio`
  are now driven by a new `ForgeEvent::RtcpReportReceived` event
  forge-engine emits on every received RR (forge-media#57 +
  #60). Closes the pre-existing 0.2.0 gap where both fields were
  always `null`. New `siphon_ai_rtp_rtt_ms` histogram alongside
  the existing jitter / loss histograms.

- **`rtcp_rtt_ms` field reserved + sticky semantics** in
  PROTOCOL §3.8. The field is documented and the wire shape is
  pinned, but stays `null` in 0.3.0 — populating it needs
  forge-engine to originate its own RTCP SRs (the
  `forge_rtp::RttTracker` primitive is ready and tested in
  forge-media#57). When a real value does arrive in a future
  release, it'll be "sticky": once populated, a later window
  with no fresh RR doesn't wipe it.

### Changed

- **`forge-media` rev pinned to `f7cd7f0`**, picking up DTLS-SRTP
  scaffolding (#61), recv-loop demux (#62), RtcpReportReceived
  event + emitter (#57 + #60), SDES primitives (#56), tarpaulin
  coverage fix (#59).

- **`siphon-rs` rev pinned to `d0d3691`**, picking up swappable
  TLS `ServerConfig` (#49) and CI-on-PR gating (#50).

- **`[sip.tls]` callout in `docs/CONFIG.md`** — old "Inbound UAS
  only" warning replaced with a precise statement: inbound UAS
  still terminates TLS here; outbound TLS works for
  `[[register]]` as of 0.3.0; originated INVITEs are still
  post-v1.

### Fixed

- **forge-rtp DTLS verify-callback** (forge-media#61). The
  existing `DtlsContext::new` installed OpenSSL's default
  chain-verify mode, which fails closed on self-signed certs —
  which is what every DTLS-SRTP peer presents (RFC 5763 §5).
  Replaced with a `set_verify_callback` that accepts any chain;
  fingerprint verification runs post-handshake as before. Makes
  the entire DTLS path actually usable for the first time.

- **forge-media Code Coverage** (forge-media#59). Tarpaulin
  failures on every PR since 2026-05-11 fixed: one missing
  feature gate (`test_codec_config_stored` needed
  `#[cfg(feature = "opus")]`) + one timing-tight assertion in
  `test_jitter_buffer_timing` that fell over under ptrace
  instrumentation. Three pre-existing dead-code `opus` tests in
  `forge-api` now actually run thanks to a new
  `forge-api/opus` feature.

### Known limitations (0.3.1 carry-forwards)

These are documented in `DEV_PLAN_0.3.0.md` §11 slip-mitigation,
`PROTOCOL.md`, and `REGISTRATION.md`:

- **`rtcp_rtt_ms` not populated end-to-end.** The field is
  reserved and the consumer wiring works, but forge-engine
  doesn't yet originate its own RTCP SRs. The `RttTracker`
  primitive is ready upstream; what's missing is the periodic
  SR send loop with RFC 3550 §6.2 bandwidth budget tracking.

- **SDES (`RTP/SAVP`) not produced.** forge-sdp ships the
  `a=crypto:` parser (forge-media#56); forge-engine doesn't
  consume it yet. SAVP / non-DTLS SAVPF offers are 488'd under
  any `srtp_mode`.

- **Per-route `[route.bridge.tls]` override.** mTLS for the
  bridge is global only in 0.3.0; every accepted call shares
  the same client cert.

- **Hostname `[[register]].server`.** Static-IP validation in
  `compile_registers` still rejects hostnames; lifting it needs
  a `RegisterConfig.server_addr: SocketAddr` refactor.

- **Per-registration cert pinning** (`[[register]].tls.pinned_sha256`).
  siphon-rs's UAC takes a daemon-wide TLS client config and
  doesn't yet expose a per-target `ClientConfig` API.

- **Attended transfer (REFER with Replaces)** carried over from
  0.2.0 — depends on a siphon-rs UAC capability that's still
  pending.

### Stats

- 8 PRs merged on siphon-ai for 0.3.0: #83, #85, #86, #87, #88,
  #89, #90, #91, #92.
- 6 upstream PRs merged on forge-media: #56, #57, #59, #60, #61,
  #62.
- 2 upstream PRs merged on siphon-rs: #49, #50.
- Workspace test count: 429 → 466 (+37 new tests across the
  sprint; every PR landed with `fmt --check` + `clippy
  --workspace --all-targets -- -D warnings` clean).

## [0.2.0] - 2026-05-25

Second release. Theme: **operator primitives** — the WS server can
now react to silence and dead-air with built-in events instead of
running its own VAD timers, observe RTP quality without scraping
RTCP, mute the AI's playout independently of `clear`, and pick
between three call-progress modes per deployment. Plus an
end-to-end Twilio recipe, a Deepgram transcription reference
server, a CI gate on every PR, and the operator-facing TLS
deployment recipe.

Protocol stays at `version: "1"` — every new variant is additive,
so v1 WS servers built against 0.1.0 keep working unchanged.

### Added

- **Transcription reference WS server** (`examples/transcription-server-py/`). Streaming Python WS server that pipes every call's audio to Deepgram and emits one JSON-line transcript per result on stdout. Demonstrates the non-agent (observer) use case — real-time transcription, compliance recording, supervisor assist. README documents the swap pattern for AssemblyAI / Whisper / OpenAI; points at `openai-realtime-bridge-py` for the multi-provider abstraction. Single dep (`websockets>=13`); ~390 LoC including comments.

- **CI workflow** (`.github/workflows/test.yml`). Gates every PR and every push to main on `fmt + clippy -D warnings + cargo test --workspace` and a follow-up `SIPp signaling regression` job that builds the daemon, brings up the echo-ws-server, and runs `test-harness/sipp-scenarios/run-all.sh`. SIPp depends on lint-and-test so a broken build doesn't burn a SIPp spin-up. Cargo cache via `Swatinem/rust-cache@v2`; toolchain comes from `rust-toolchain.toml`. `run-all.sh` is now `DAEMON_BIN`-env-overridable so CI / operators can point at a release build or a custom path without editing the script.

- **Twilio Elastic SIP Trunking integration recipe**. `docs/INTEGRATIONS_TWILIO.md` walks the trunk-side setup (Origination URI, signalling-IP allowlist, TLS) and the siphon-ai-side config end-to-end; the Programmable Voice `<Dial><Sip>` flow gets a brief alternative section with a TwiML snippet. Runnable starter config at `examples/twilio-trunk/`.

- **`rtp_stats` WS event + RTP-quality histograms** (PROTOCOL §3.8). Periodic snapshot of RTP-quality state cached from forge `QualityDegraded` / `QualityRestored` events. Cadence configurable via `[bridge].rtp_stats_interval_ms` (default `5000`, mirroring RTCP §6.2; per-route override; `0` disables). Fields `jitter_ms` / `packet_loss_ratio` are `null` until forge reports a first assessment; `QualityRestored` resets them to `0.0` (distinct from `null`). Two histograms — `siphon_ai_rtp_jitter_ms`, `siphon_ai_rtp_packet_loss_ratio` — record values on every emission. HEP RTCP chunks (forge-hep) already ship to the configured collector — no extra wiring needed. `rtcp_rtt_ms` is not yet exposed (forge upstream gap; deferred to 0.2.1 / 0.3.0). New `RtpStatsTracker` helper with 7 unit tests.

- **`silence_detected` / `dead_air_detected` WS events** (PROTOCOL §3.6 / §3.7). Timer-derived primitives the WS server can use for "are you still there?" prompts and hung-call teardown. `silence_detected` is one-sided (caller has been VAD-silent past `[bridge].silence_threshold_ms`, default 3 s); fires once per silence stretch. `dead_air_detected` is two-sided (neither caller speech nor outbound WS audio past `[bridge].dead_air_threshold_ms`, default 10 s); re-fires on every elapsed threshold. Both thresholds are per-route overridable; `0` disables. Detection cadence is 500 ms. Underlying state machine factored into `IdleDetector` (8 unit tests). Counters: `siphon_ai_silence_events_total`, `siphon_ai_dead_air_events_total`.

- **`BridgeIn::Mute` / `BridgeIn::Unmute`** (WS protocol §4.6). Sustained AI-side mute primitive — distinct from `clear` (one-shot flush). On `mute`: subsequent audio bytes from the WS server are dropped (channel still drained so the WS server isn't back-pressured) and forge's playout queue is flushed for immediate silence. `unmute` releases the gate. Protocol-version unchanged; existing servers ignore the new variants.

- **Configurable SIP call progress** (`[sip.call_progress]`). New `mode` field selects what — if any — provisional response the UAS sends before the `200 OK`:
  - `instant_answer` (default; v0.1.0 behaviour): skip extra provisionals.
  - `ringing`: send `180 Ringing` (no body) before the 2xx.
  - `session_progress`: send `183 Session Progress` with the negotiated answer SDP before the 2xx (Flavour B per `docs/design/DEV_PLAN_0.2.0.md` §9.1 — best-effort, no `100rel`). Peers that include `Require: 100rel` in the INVITE fall back to `instant_answer` with a `warn!` log; reliable provisionals are deferred to 0.2.1 / 0.3.0.

  Backwards-compatible: existing configs without the `[sip.call_progress]` block keep v0.1.0 behaviour.

- **TLS deployment recipe** (`docs/DEPLOY.md` § TLS deployment). End-to-end walkthrough for a TLS-secured deployment using the SIP/TLS + WSS mechanics that already shipped in 0.1.0: cert provisioning options, `[sip.tls]` configuration, the file-permission pattern for cert/key under the systemd `siphon` user, Let's Encrypt deploy-hook for renewal, and an `openssl s_client` + SIPp `-t l1` smoke test. WSS works out-of-the-box against any publicly-signed cert because the WS client is built with `rustls-tls-webpki-roots` — no host-CA-store dependency.

### Changed

- **Rust toolchain pinned to `1.95.0`** (`rust-toolchain.toml`). Previously `channel = "stable"`, which let local dev clippy drift from CI clippy — a drift PR #78 surfaced when CI's clippy 1.95.0 caught a `result_large_err` lint that the older local clippy was silent on. Future-stable bumps are now an explicit edit to this file.

- **CI failure diagnostics for SIPp** (`.github/workflows/test.yml`). The SIPp regression job now cats every `*_errors.log` (in the scenarios dir; `run-all.sh` pins its CWD there so paths are predictable) and every daemon log on failure. The first real failure under the new pipeline — a `session_timer_echo` SIPp scenario using `[auto_media_port]` (added in SIPp 3.7; CI's ubuntu-latest apt sip-tester is 3.6.0) — was diagnosed and fixed in the same hour the dump was added.

### Known limitations

These are documented because they're DoD adjacent and worth setting expectation around.

- **`rtp_stats.rtcp_rtt_ms` is not populated.** The `rtp_stats` event has the field reserved in PROTOCOL §3.8, but jitter and packet-loss are the only quality dimensions the daemon currently exposes (forge-media doesn't surface RTT in the `QualityDegraded` / `QualityRestored` events the snapshot is derived from). RTT exposure is targeted at 0.2.1 / 0.3.0 alongside the forge-media work.
- **Reliable provisionals (RFC 3262 `100rel`) for `session_progress` mode** are not implemented. INVITEs that include `Require: 100rel` fall back to `instant_answer` for that call with a `warn!` log rather than sending a non-compliant unreliable 183. The reliable path is paired with `BridgeIn::Answer` (the "AI plays during the 183 phase" flow) for 0.2.1 / 0.3.0.
- **Hot reload of the SIP/TLS cert is not implemented.** Cert rotation requires a daemon restart; pair with an L4 load balancer if your traffic pattern can't tolerate that. The renewal recipe in `docs/DEPLOY.md` § TLS deployment uses a Let's Encrypt deploy-hook + `systemctl restart`.

### Deferred to 0.2.1 (Sprint 1 §5 stretch slip)

`docs/design/DEV_PLAN_0.2.0.md` §5 listed three stretch items that slip to 0.2.1 per the plan's own policy ("Stretch items slot into spare time, in §5 order. If stretch eats more than Week 5, bump them to 0.2.1."). For clarity:

- **mTLS for the bridge WebSocket connection** and wire-format validation against the WS server's cert. The 0.2.0 TLS recipe in `docs/DEPLOY.md` covers SIP/TLS + server-auth WSS + cert rotation; client-cert auth on the WS leg would need a `[bridge.tls.client_cert]` / `[bridge.tls.client_key]` config surface and the matching rustls connector wiring — not in 0.2.0.
- **Attended transfer (REFER with Replaces)** — depends on siphon-rs UAC capability that wasn't ready in time.
- **`examples/provider-toolkit-py/`** — a pluggable Deepgram/Whisper STT + OpenAI/Anthropic/Groq LLM + ElevenLabs/Cartesia TTS reference example. The 0.2.0 reference servers (`echo-ws-server-python`, `openai-realtime-bridge-py`, `transcription-server-py`) cover the canonical shapes; the multi-provider toolkit is a 0.2.1 cleanup item.

## [0.1.0] - 2026-05-22

First public release. SiphonAI is a provider-neutral SIP-to-WebSocket
media bridge: it terminates SIP calls, streams the call audio over a
WebSocket to a developer-supplied server, and plays audio received back
over that WebSocket into the call. It contains no AI code — the AI is
the WebSocket server's job.

### Added

#### SIP signaling

- Inbound trunk mode (UAS): accept calls from a SIP trunk or PBX, gated
  by an optional per-trunk source-IP / From-host allowlist.
- Registered-phone mode (UAC + REGISTER): register to a PBX (e.g. Cisco
  CUCM, Asterisk, FreeSWITCH) as a phone, with periodic re-REGISTER,
  retry/backoff, and digest authentication.
- Call lifecycle: INVITE / ACK / BYE / CANCEL, 100 Trying, provisional
  and final responses, re-INVITE for hold / resume.
- Blind transfer initiated from the WebSocket server (REFER).
- RFC 3261 / RFC 3581 response compliance: Via `received=` / `rport=`,
  rich Contact, and an honest `Allow` header on 405 / OPTIONS.

#### Media

- RTP / RTCP bridging via forge-media, with jitter buffering.
- Codecs: G.711 PCMU / PCMA (8 kHz) and G.722 (16 kHz).
- DTMF via RFC 2833 (telephone-event), surfaced to the WebSocket server.
- Barge-in: VAD-driven `speech_started` events for interruption handling.

#### WebSocket bridge protocol v1

- Bidirectional audio as 20 ms PCM16 little-endian mono frames
  (160 samples @ 8 kHz, 320 @ 16 kHz).
- Control and event messages with monotonic per-call `seq` numbering.
- Canonical protocol specification in `docs/PROTOCOL.md`.

#### Routing

- TOML dialplan: ordered, first-match-wins routes matched on the inbound
  INVITE (request URI, To, From, Call-ID, custom headers).
- Optional per-route regex matching and per-route overrides of global
  media / bridge settings.

#### Configuration

- Single TOML configuration file with load-time validation (invalid
  regex, dangling references, unset env vars fail loud at startup).
- Environment-variable expansion in config values.

#### Observability

- Structured `tracing` logs with `call_id` correlation.
- Prometheus metrics with bounded-cardinality labels.
- Distributed tracing spans for long-running per-call operations.
- HEP/EEP emission to Homer for SIP, RTCP, and application events.
- Call Detail Records (CDR) as JSON, to a file sink and/or webhook sink.
- Out-of-band lifecycle webhooks (call start / end, registration state).
- `/health` and `/ready` endpoints with k8s-correct semantics.
- Runtime per-target log-level adjustment via the admin API.

#### Packaging

- Multi-stage Docker image and `docker compose` quickstart stack.
- Idempotent Debian 13 install scripts with systemd units.
- Reference WebSocket servers in `examples/`: echo (Python / Node),
  an OpenAI Realtime bridge, and a Deepgram + LLM voice bot.

[Unreleased]: https://github.com/thevoiceguy/siphon-ai/compare/v0.14.1...HEAD
[0.14.1]: https://github.com/thevoiceguy/siphon-ai/compare/v0.14.0...v0.14.1
[0.14.0]: https://github.com/thevoiceguy/siphon-ai/compare/v0.13.0...v0.14.0
[0.13.0]: https://github.com/thevoiceguy/siphon-ai/compare/v0.12.2...v0.13.0
[0.6.2]: https://github.com/thevoiceguy/siphon-ai/compare/v0.6.1...v0.6.2
[0.6.1]: https://github.com/thevoiceguy/siphon-ai/compare/v0.6.0...v0.6.1
[0.6.0]: https://github.com/thevoiceguy/siphon-ai/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/thevoiceguy/siphon-ai/compare/v0.4.1...v0.5.0
[0.4.1]: https://github.com/thevoiceguy/siphon-ai/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/thevoiceguy/siphon-ai/compare/v0.3.2...v0.4.0
[0.3.2]: https://github.com/thevoiceguy/siphon-ai/compare/v0.3.1...v0.3.2
[0.3.1]: https://github.com/thevoiceguy/siphon-ai/compare/v0.2.0...v0.3.1
[0.2.0]: https://github.com/thevoiceguy/siphon-ai/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/thevoiceguy/siphon-ai/releases/tag/v0.1.0
