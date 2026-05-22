# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
