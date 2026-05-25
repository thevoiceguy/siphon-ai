# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
