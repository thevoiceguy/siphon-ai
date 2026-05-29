# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed

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
  - `session_progress`: send `183 Session Progress` with the negotiated answer SDP before the 2xx (Flavour B per `docs/DEV_PLAN_0.2.0.md` §9.1 — best-effort, no `100rel`). Peers that include `Require: 100rel` in the INVITE fall back to `instant_answer` with a `warn!` log; reliable provisionals are deferred to 0.2.1 / 0.3.0.

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

`docs/DEV_PLAN_0.2.0.md` §5 listed three stretch items that slip to 0.2.1 per the plan's own policy ("Stretch items slot into spare time, in §5 order. If stretch eats more than Week 5, bump them to 0.2.1."). For clarity:

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

[Unreleased]: https://github.com/thevoiceguy/siphon-ai/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/thevoiceguy/siphon-ai/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/thevoiceguy/siphon-ai/releases/tag/v0.1.0
