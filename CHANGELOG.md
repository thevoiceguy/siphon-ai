# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

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

[Unreleased]: https://github.com/thevoiceguy/siphon-ai/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/thevoiceguy/siphon-ai/releases/tag/v0.1.0
