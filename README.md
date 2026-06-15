# SiphonAI

A SIP-to-WebSocket media bridge written in Rust.

SiphonAI accepts inbound SIP calls (as a trunk endpoint or as a registered
phone on a PBX), streams the call's audio over a WebSocket to a developer-
supplied server, and plays audio received back over that WebSocket into the
call. **It does not contain any AI code** — that is the WebSocket server's job.

## How it fits together

```mermaid
flowchart LR
    Caller([SIP caller<br/>softphone / trunk / PBX])
    WS["Your WS server<br/>(STT • LLM • TTS)"]
    Homer[("Homer / HEPIC<br/>HEP3 collector")]

    subgraph SiphonAI ["SiphonAI daemon"]
        direction LR
        sip["siphon-rs<br/>SIP UAS / UAC"]
        forge["forge-media<br/>RTP • codecs • SDP"]
        bridge["bridge crate<br/>WS protocol v1"]
        sip -- "INVITE / BYE / REFER" --> ctrl
        forge -- "PCM16 20 ms frames" --> ctrl
        ctrl["core::CallController<br/>per-call state machine"]
        ctrl --> bridge
    end

    Caller -- "SIP + RTP" --> sip
    forge <-- "RTP" --> Caller
    bridge <== "WebSocket<br/>JSON ctrl + binary audio" ==> WS

    sip -. "HEP3 SIP (0x01)" .-> Homer
    forge -. "HEP3 RTCP (0x05) + QoS (0x20)" .-> Homer
    ctrl -. "HEP3 CDR (0x65)" .-> Homer
```

The WebSocket server runs the AI — STT, LLM, TTS, whatever fits the
use case. SiphonAI is the bridge: SIP signaling, RTP media, codec
handling, jitter, barge-in, DTMF, hold, transfer. See
[`docs/PROTOCOL.md`](docs/PROTOCOL.md) for the contract.

## Status

**v0.7.0** — eighth release. Theme: **conferencing + media-only call park.**
Two operator-controllable multi-leg features, both **off by default**.
Conferencing mixes N calls into one room where *every* leg keeps its own WS
session (no single "host" bot) and each side hears the mix minus its own
input; a WS server joins its own call (`conference_join`/`leave`), and
operators compose rooms over `/admin/v1/conferences` (add/remove **any**
active call, cross-call via a new bridge-id → `CallHandle` registry that keeps
CLAUDE §4.4 intact). Call park shelves a call on hold music with **no** WS
session — `park` detaches the bot, the SIP dialog + RTP stay up, and an
operator **retrieves** it later onto a *fresh* session (`start.retrieved`),
or it times out (`hangup`|`keep`). The protocol stays `version: "1"` (new
messages/events/error codes only); a 0.6.x deployment upgrades with zero
behaviour change. See [`docs/CONFERENCE.md`](docs/CONFERENCE.md) and
[`docs/PARK.md`](docs/PARK.md). Full notes: [`CHANGELOG.md`](CHANGELOG.md).

**v0.6.2** — patch release. Theme: **TLS trunk hardening.** Fixes found by
running v0.6.1 against a production TLS trunk: the `Contact` on dual-listener
daemons advertised the UDP port for TLS dialogs (losing in-dialog ACK/BYE —
~60 s silent-tail recordings, wrong CDR cause), and daemon-initiated BYE and
REFER on TCP/TLS dialogs dialed fresh connections nothing answered — both now
reuse the inbound connection (RFC 5626 flow semantics). The transport
dispatcher also grows **outbound TCP/TLS**: `[[gateway]]` / `[[register]]`
blocks with `transport = "tcp" | "tls"` dial out through client connection
pools, verifying peers against the bundled webpki roots plus an optional
`[sip.tls_client].extra_ca` (signaling only — SRTP media is a follow-up). The
Deepgram/LLM example bot gains human-handoff transfer triggers (keyword
fast-path + a `transfer_call` LLM tool). Protocol stays `version: "1"`; CDR
schema unchanged; a 0.6.1 deployment upgrades with zero config changes. Full
notes: [`CHANGELOG.md`](CHANGELOG.md).

**v0.6.1** — seventh release. Theme: **attended transfer.** The bot
consults a human before handing the caller off: SiphonAI places the
consult leg as a plain 0.6.0 outbound call (`POST /admin/v1/calls`, its
own WS session), and the WS server completes the handoff with one
additive protocol field — `transfer { replaces_call_id }` — which becomes
a REFER-with-Replaces on the original call (RFC 5589), connecting the two
humans directly. The `Refer-To` is derived from the consult dialog
(explicit `target` overrides), outbound legs are transferable too, and
both transfer modes now emit `siphon_ai_transfers_total{mode,result}`.
Builds on 0.6.0's outbound origination: gateways, the originate API, the
toll-fraud posture (private bind + reverse proxy + cap + rate limit), and
`start.direction` are unchanged — see
[`docs/OUTBOUND.md`](docs/OUTBOUND.md) and
[`docs/PROTOCOL.md`](docs/PROTOCOL.md) §4.4. Full notes:
[`CHANGELOG.md`](CHANGELOG.md).

**v0.5.0** — fifth release. Theme: **call recording.** Each call's audio can
be captured to a stereo WAV (caller left, bot/WS right) for compliance and
QA — `[recording].mode` = `off` (default) / `always` / `on_demand`, with a
per-route `[route.recording]` override. On-demand recording is driven over
the WS protocol (`start`/`stop`/`pause`/`resume_recording`; a pause *omits*
the span — the PCI primitive), and surfaced on the CDR (`recording_id` /
`recording_path`) and a `siphon_ai_recordings_total` metric. The recorder
runs off the audio hot path — a backed-up writer drops frames (`degraded`)
rather than ever stalling the live call. **Off by default**; a 0.4.x
deployment upgrades with zero behaviour change. Protocol stays `version: "1"`
and the CDR schema stays at version 1 (both additions are additive). See
[`docs/RECORDING.md`](docs/RECORDING.md). (A timed SRTP re-key was planned to
ride along but was deferred — forge-media has no coordinated re-key API.)
Full notes: [`CHANGELOG.md`](CHANGELOG.md).

**v0.4.1** — patch release completing the 0.4.0 STIR/SHAKEN theme: PASSporT
`iat` freshness (replay protection, `verstat.iat_passed`), an
`x5u_tls_extra_ca` knob for privately-hosted `x5u`, the security-model doc
([`docs/SECURITY_STIR_SHAKEN.md`](docs/SECURITY_STIR_SHAKEN.md)), a Twilio
`X-Twilio-VerStat` cross-check recipe, and the first CI-gated *passing*
attestation scenario. Still off by default; protocol stays `version: "1"`.
Full notes: [`CHANGELOG.md`](CHANGELOG.md).

**v0.4.0** — fourth release. Theme: **STIR/SHAKEN call authentication.**
Inbound INVITEs with an RFC 8224 `Identity` header are verified end-to-end
(PASSporT/RFC 8225 decode, ES256, X.509 chain to a configured STI-PA trust
anchor via the `x5u` cert with a TTL cache, and the `orig`/`dest` ↔
`From`/`To` claim checks), producing a per-call *verstat* verdict. Operators
can gate on it — `min_attestation` (403/488/606) and `require_identity`
(428), with per-route overrides — and the verdict is surfaced on the WS
`start` message, the CDR, a structured log line, and a new HEP3 chunk
(`0x66`) for Homer. Everything is **off by default**: a 0.3.x deployment
upgrades with zero behaviour change until `[security.stir_shaken].enabled
= true`. Protocol stays at `version: "1"` — `start.verstat` is additive, so
v1 WS servers built against earlier releases keep working unchanged. Full
notes: [`CHANGELOG.md`](CHANGELOG.md).

**v0.3.2** — patch release. `rtp_stats.rtcp_rtt_ms` now populates on live
calls (forge-engine originates RTCP Sender Reports for its generated
streams, so the carrier's Receiver Reports resolve a round-trip time per
RFC 3550 §A.7) — closing the last open 0.3.0 item. No protocol or config
change; the `rtp_stats` WS field and `siphon_ai_rtp_rtt_ms` histogram
simply start carrying real values. Full notes: [`CHANGELOG.md`](CHANGELOG.md).

**v0.3.1** — third release. Theme: **trust & encryption**, hardened for
real carriers. Every transport the daemon touches can now run encrypted:
SRTP media (SDES `a=crypto:` for classic SIP trunks **and** DTLS-SRTP for
WebRTC bridges), mTLS with optional SPKI pinning on the bridge WebSocket
leg, hot-reloadable SIP/TLS certs (`systemctl reload`, no in-flight call
drops), and REGISTER over TLS. Validated end-to-end against a Twilio
Elastic SIP Trunk's Secure Trunking (TLS + SRTP), including a round of
SRTP/SRTCP/RTCP spec-conformance fixes that only surface against a
spec-correct carrier. Protocol stays at `version: "1"` — additive only,
so v1 WS servers built against 0.1.0 / 0.2.0 keep working unchanged.
(0.3.0 was prepared but never tagged; its features ship here, hardened.)
Full notes: [`CHANGELOG.md`](CHANGELOG.md).

**v0.2.0** — second release. Adds the operator-primitive event surface
(`silence_detected`, `dead_air_detected`, `rtp_stats`), sustained-mute
control (`BridgeIn::Mute` / `Unmute` — distinct from one-shot `clear`),
three configurable call-progress modes (`instant_answer` / `ringing` /
`session_progress`), an end-to-end Twilio Elastic SIP Trunk recipe, a
Deepgram transcription reference WS server, a CI gate on every PR
(fmt + clippy + cargo test + SIPp regression), and the operator-facing
TLS deployment recipe. Protocol stays at `version: "1"` — every new
variant is additive, so v1 WS servers built against 0.1.0 keep working
unchanged. Full notes: [`CHANGELOG.md`](CHANGELOG.md).

**v0.1.0** — first public release. The audio path, WS protocol, SIP signaling,
HEP capture, CDR/webhook sinks, and trunk-allowlist gate are in. Real-world
FreeSWITCH-bridged calls work with sub-second user-to-audio latency on a
Groq LLM. A 500-concurrent burst soak completes with no rejections and no
RSS growth past the working-set; a 1-hour long-call soak holds RSS within
±32 KB.

See `docs/DEV_PLAN.md` for what's deliberately out of scope for v1
(recording, mid-call WS reconnect, multi-tenancy, video).

## Scope

SiphonAI is the bridge layer between SIP and a WebSocket. The contract is:

| | SiphonAI provides | Your responsibility |
|---|---|---|
| SIP signaling, RTP, codecs, jitter | ✓ | — |
| WebSocket bridge protocol (`docs/PROTOCOL.md`) | ✓ | — |
| Speech-start / DTMF / hold / resume events on the WS | ✓ | — |
| Auto-clear of daemon-side playout on barge-in | `[bridge.barge_in].mode` config | — |
| STT, LLM, TTS, conversation logic | — | ✓ |
| What to do with a `speech_started` event | — | ✓ |
| Acoustic echo cancellation | — | ✓ (handset or AEC) |

The reference bot in `examples/deepgram-llm-bot-node/` is a working demo
of the protocol, not the product. Tune it for your deployment or replace
it entirely with your own WS server in any language.

## Quickstart (Docker)

The fastest way to see SiphonAI work end-to-end is the local demo
stack — a containerized daemon talking to the reference Python echo
WebSocket server.

```sh
# Build + run the daemon + echo-ws in the background.
docker compose -f docker/compose.yaml up -d

# Drive a call from your host. Any softphone pointed at
# 127.0.0.1:5070 works; this one uses SIPp.
sipp -sf test-harness/sipp-scenarios/basic_call_then_bye.xml \
     -m 1 -p 5080 -s 1000 127.0.0.1:5070

# Watch the call land:
docker compose -f docker/compose.yaml logs -f siphon-ai echo-ws
```

The echo server replays every audio frame back into the call, so if
you use a softphone you'll hear yourself.

The compose file mounts `docker/local-dev.toml` over the image's
default config. Edit it (or supply your own with `-v
./my.toml:/app/config.toml:ro`) and `docker compose restart
siphon-ai` to apply.

Prometheus metrics live on `http://127.0.0.1:9091/metrics`;
`/health` and `/ready` are next to them.

For the full HEP/Homer end-to-end demo (SIP + RTCP + CDRs
correlated in one call view), see
[`examples/homer-stack/`](examples/homer-stack/).

## Production install (Debian 13)

Two scripts walk through `docs/INSTALL_DEBIAN13.md` and
`docs/BOT_LOCALHOST_SETUP.md` end-to-end. Both are idempotent —
re-running is safe.

```sh
git clone https://github.com/thevoiceguy/siphon-ai.git /opt/siphon-ai-src
cd /opt/siphon-ai-src

# Daemon: packages, rustup, build, systemd unit, working TOML config.
TRUNK_PEER_IP=<FreeSWITCH-or-ITSP-IP> ./scripts/install-debian13.sh

# Reference bot: Node 22, npm install, env file, systemd unit,
# optional daemon ws_url repoint.
DEEPGRAM_API_KEY=dg_xxx OPENAI_API_KEY=sk-xxx \
    ./scripts/install-bot-debian13.sh
```

Trunking to FreeSWITCH? Read `docs/FREESWITCH_INTEGRATION.md` —
the `bypass_media=true` dialplan setting is **required** for
clean audio when the softphone offers `a=rtcp-mux` (most do).

## Reference bot (`examples/deepgram-llm-bot-node/`)

Working closed-loop voice agent in Node:
caller → Deepgram STT → streaming LLM → Deepgram TTS → caller.

The LLM is any OpenAI-compatible chat-completions endpoint,
selected via env vars at startup:

| Provider | `BOT_LLM_BASE_URL` |
|---|---|
| OpenAI (default) | (unset) |
| Groq | `https://api.groq.com/openai/v1` |
| Anthropic | `https://api.anthropic.com/v1/` |
| OpenRouter | `https://openrouter.ai/api/v1` |
| Local Ollama | `http://127.0.0.1:11434/v1` |

In practice with Groq + `llama-3.3-70b-versatile`,
user-stop-to-agent-audio runs ~600 ms steady-state, with the
~1 s floor from Deepgram's `utterance_end_ms` minimum.

Per-turn latency, LLM/TTS timings, barge-in counts, and dropped
frames are emitted as one-line `metric` records in the bot's
journal — grep `turn_summary` for SLO numbers. See
`docs/BOT_LOCALHOST_SETUP.md` §4.

## Layout

| Path | Purpose |
|---|---|
| `crates/core/`        | `CallController`, state machine, glue |
| `crates/bridge/`      | WS client + protocol types + audio bridging |
| `crates/sip-glue/`    | Adapter from `siphon-rs` events to core |
| `crates/media-glue/`  | Adapter from `forge-engine` to core (the audio tap) |
| `crates/routes/`      | Route matching engine (TOML dialplan) |
| `crates/cdr/`         | CDR generation (JSON), file sink, webhook sink |
| `crates/webhooks/`    | Out-of-band lifecycle webhooks |
| `crates/config/`      | TOML config + validation + reload |
| `crates/telemetry/`   | tracing + metrics + HEP wiring + admin endpoints |
| `bins/siphon-ai/`     | The daemon binary |
| `examples/`           | Reference WS servers and the local Homer stack |
| `scripts/`            | Idempotent Debian 13 install scripts (daemon + bot) |
| `test-harness/`       | SIPp scenarios, load tooling, HEP collector stub |
| `docs/`               | Protocol, config, dialplan, HEP, deployment, FS integration |

## Building

```sh
cargo build --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

## Observability

| Surface | URL / location |
|---|---|
| Liveness / readiness | `GET /health`, `GET /ready` |
| Prometheus metrics | `GET /metrics` |
| Active calls | `GET /admin/calls` |
| Per-call hangup | `POST /admin/calls/<id>/hangup` |
| Runtime log filter | `PUT /admin/log` |
| HEP test packet | `POST /admin/hep/test` |
| CDR (JSONL file) | `/var/log/siphon-ai/cdr.jsonl` |
| Lifecycle webhooks | `[webhooks]` block in the TOML |
| Full SIP + RTCP + CDR correlation | HEP → Homer (see `docs/HEP.md`) |

**The admin endpoints have no built-in auth** (per the v1 threat
model in `crates/telemetry/src/admin.rs`). The shipped Docker
compose binds them to `127.0.0.1` only; for any other deployment
sit them behind an authenticating reverse proxy.

## Reading order for contributors

1. `CLAUDE.md` — operating instructions (read first; re-check before
   non-trivial changes).
2. `docs/DEV_PLAN.md` — what we're building and why.
3. `docs/PROTOCOL.md` — the WebSocket bridge protocol contract.
4. `docs/INSTALL_DEBIAN13.md` + `docs/FREESWITCH_INTEGRATION.md` +
   `docs/BOT_LOCALHOST_SETUP.md` — the deploy-the-thing path.
5. `docs/CONFIG.md` — every TOML field documented.
6. `docs/RECORDING.md` — call recording (stereo WAV, on-demand control, CDR).

## Upstream dependencies

SiphonAI is glue. The heavy lifting lives in three companion repos owned by
the same author:

- [`siphon-rs`](https://github.com/thevoiceguy/siphon-rs) — RFC 3261 SIP stack
- [`forge-media`](https://github.com/thevoiceguy/forge-media) — RTP/codecs/SDP/jitter/VAD
- [`hep-rs`](https://github.com/thevoiceguy/hep-rs) — HEP3 codec, transport, `HepSink` trait

## License

Dual-licensed under MIT or Apache-2.0, matching the upstream stack.
