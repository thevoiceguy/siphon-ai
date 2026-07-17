# SiphonAI

A SIP-to-WebSocket media bridge written in Rust.

**AI was used to help write this application**

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

**Current release: v0.37.0.** Production-deployed against real carriers
(Twilio Elastic SIP Trunking, FreeSWITCH, CUCM). The WS protocol is still
`version: "1"` — every release has been additive, so a WS server built
against 0.1.0 keeps working unchanged, and upgrading the daemon is a
behaviour-preserving drop-in (features below are **off by default** until
you turn them on; the one operator-facing move was v0.10.0 relocating
`/admin/*` onto its own authenticated listener). The full per-release
history is in [`CHANGELOG.md`](CHANGELOG.md).

### What's shipped

On top of the core bridge — SIP signaling, RTP, jitter buffer, barge-in,
DTMF, and the WebSocket protocol:

- **Codecs & media** — G.711 (µ-law/A-law) and **Opus** (negotiated at a
  16 kHz bridge rate); **SRTP both directions** — SDES (`a=crypto:`) and
  DTLS-SRTP, inbound and outbound; offerless / delayed-offer INVITEs (CUCM,
  avoids a forced MTP); selectable **speech detection backend** —
  energy/ZCR (default) or the **Silero neural VAD** (local inference, no
  network) for fewer acoustic false positives, per-route via `[media].vad`.
- **Call control** — bot-initiated **hold/resume** (true SIP re-INVITE +
  hold music), blind and **attended transfer** (REFER / REFER-with-Replaces),
  **N-way conferencing** (mixed rooms, every leg keeps its WS), and media-only
  **call park** (retrieve onto a fresh session).
- **Outbound origination** — `POST /admin/v1/calls` with `[[gateway]]`
  trunks, toll-fraud guardrails (cap + rate limit), and outbound SRTP.
- **Recording** — per-call stereo WAV or **Opus** (`off` / `always` /
  `on_demand`, PCI-aware pause), written off the audio hot path; inbound
  **and outbound** legs; **encryption at rest** (envelope format + offline
  decrypt CLI, local KEK or **AWS KMS**), **S3 storage**, and a **consent
  announcement** ("this call may be recorded" prompt gates capture, consent
  stamped into the CDR). See [`docs/RECORDING.md`](docs/RECORDING.md).
- **Reliability** — mid-call **WS reconnect**: an unexpected WS drop parks the
  caller on hold music, re-dials, and resumes on a fresh session; or play a
  configurable **failure prompt** to the caller before hanging up
  (`on_ws_failure = "play_prompt"`).
- **Security** — **STIR/SHAKEN** verification + policy gate; **native admin
  auth + RBAC** (bearer tokens, nested `readonly` ⊂ `operator` ⊂ `admin`
  roles) on a dedicated authenticated listener, with TLS and `SIGHUP` cert
  reload; SIP **digest auth** and **admission control** (allowlists, rate
  limits) on the inbound leg; per-route **bridge mTLS** on the WS leg;
  `${file:}` / `${cred:}` **secret resolution** so tokens stay out of the
  TOML; and a **signed audit-event stream** (`docs/AUDIT.md`).
- **Delivery durability** — webhook + CDR **HMAC signing**
  (`X-SiphonAI-Signature`), per-event idempotency ids, and a **durable retry
  spool** that survives daemon restarts.
- **Observability** — Prometheus metrics + **Grafana dashboards and alert
  rules as code** (`examples/observability/`), daemon-side **OTLP trace
  export** (one trace per call), and **W3C trace-context propagation** to
  your WS server so its spans join the call's trace; **per-call quality
  telemetry** — receive-side jitter/loss/**MOS** in the CDR, live via
  `GET /admin/v1/calls/{id}/stats`, and streamed to the WS server as
  `rtp_stats` events; HEP3 → Homer for SIP/RTCP/CDR correlation.
- **Operations** — a config CLI (`siphon-ai check` / `print-config` /
  `route-test`) and **`SIGHUP` hot-reload** of routes, webhook/CDR sinks, and
  outbound gateways (fail-safe — a bad config is rejected and the running one
  kept; socket-binding / concurrency changes warn restart-required);
  **graceful shutdown** for zero-drop deploys (`SIGTERM` flips `/ready`,
  503s new INVITEs, drains active calls before exit); **registration
  management** over the admin API (force-refresh / restart a PBX
  registration without touching the daemon).
- **Developer surface** — the WS protocol as a machine-readable **JSON
  Schema** (`schemas/siphon-ai.v1.json`, drift-checked in CI), **server
  SDKs** for Python + TypeScript (`sdks/`) — typed events, paced 20 ms audio
  framing, connection lifecycle — and a **conformance testkit**
  (`siphon-ai-testkit`) that plays the daemon side against any WS server
  and validates its behavior (`docs/CONFORMANCE.md`).

See [`docs/DEV_PLAN.md`](docs/DEV_PLAN.md) for design rationale. Still
deliberately out of scope: multi-tenancy, video, and WebRTC client support.

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

Writing that server in Python or TypeScript? The **server SDKs** in
[`sdks/`](sdks/) handle the wire protocol for you — typed events, paced
20 ms audio framing, and connection lifecycle — so you write handlers,
not wire code. The protocol itself is also machine-readable:
[`schemas/siphon-ai.v1.json`](schemas/siphon-ai.v1.json).

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

## Production install

**From a release (recommended):** every tag ships static musl binary
tarballs (amd64 + arm64), Debian packages, and a multi-arch container on
GHCR — all cosign-signed with checksums and a CycloneDX SBOM. See
[`docs/DEPLOY.md`](docs/DEPLOY.md) → *Install from a release* for the
verify-and-install steps.

```sh
# Debian/Ubuntu: grab the .deb for your arch from
# https://github.com/thevoiceguy/siphon-ai/releases/latest — then:
sudo apt install ./siphon-ai_<version>_<arch>.deb
```

**From source (Debian 13):**
two scripts walk through `docs/INSTALL_DEBIAN13.md` and
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
| `crates/cdr/`         | CDR generation (JSON), file sink, signed/spooled webhook sink |
| `crates/webhooks/`    | Out-of-band lifecycle webhooks (signed, durable spool) |
| `crates/http/`        | Shared retrying HTTP delivery (signing, idempotency, spool) |
| `crates/config/`      | TOML config + validation + SIGHUP reload |
| `crates/telemetry/`   | tracing + metrics + HEP wiring + admin API (auth + RBAC) |
| `crates/protocol-testkit/` | `siphon-ai-testkit` — WS protocol conformance harness |
| `bins/siphon-ai/`     | The daemon binary |
| `sdks/`               | Server SDKs (Python + TypeScript) for the WS protocol |
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

| Surface | URL / location | Listener |
|---|---|---|
| Liveness / readiness | `GET /health`, `GET /ready` | `[observability]` (open) |
| Prometheus metrics | `GET /metrics` | `[observability]` (open; optional bearer token via `metrics_token`) |
| Active calls | `GET /admin/calls` | `[admin]` (auth) |
| Per-call live quality stats | `GET /admin/v1/calls/<id>/stats` | `[admin]` (auth) |
| Per-call hangup | `POST /admin/calls/<id>/hangup` | `[admin]` (auth) |
| Outbound origination | `POST /admin/v1/calls` | `[admin]` (auth) |
| Registration refresh / restart | `POST /admin/v1/registrations/<name>/refresh\|restart` | `[admin]` (auth) |
| Conference / park control | `/admin/v1/conferences`, `/admin/v1/parked` | `[admin]` (auth) |
| Runtime log filter | `PUT /admin/log` | `[admin]` (auth) |
| HEP test packet | `POST /admin/hep/test` | `[admin]` (auth) |
| Drain status (during shutdown) | `GET /admin/v1/drain` | `[admin]` (auth) |
| CDR file (JSONL or CSV) | `/var/log/siphon-ai/cdr.jsonl` | — |
| Lifecycle webhooks | `[webhooks]` block in the TOML | — |
| OTLP trace export (one trace per call) | `[observability.otlp]` in the TOML | — |
| Grafana dashboards + Prometheus alerts | `examples/observability/` | — |
| Full SIP + RTCP + CDR correlation | HEP → Homer (see `docs/HEP.md`) | — |

Since **v0.10.0** the admin API is **authenticated**: `/admin/*` is served
on a dedicated `[admin]` listener gated by bearer tokens with nested
`readonly` ⊂ `operator` ⊂ `admin` roles. `/metrics`, `/health`, and `/ready`
stay on the open `[observability]` listener; `/admin/*` returns `404` there.
Omit the `[admin]` block entirely and `/admin/*` is not served at all (the
secure default). See [`docs/DEPLOY.md`](docs/DEPLOY.md) → *Admin auth & RBAC*.

## Reading order for contributors

1. `CLAUDE.md` — operating instructions (read first; re-check before
   non-trivial changes).
2. `docs/DEV_PLAN.md` — what we're building and why.
3. `docs/PROTOCOL.md` — the WebSocket bridge protocol contract.
4. `docs/INSTALL_DEBIAN13.md` + `docs/FREESWITCH_INTEGRATION.md` +
   `docs/BOT_LOCALHOST_SETUP.md` — the deploy-the-thing path.
5. `docs/CONFIG.md` — every TOML field documented, plus the `check` /
   `print-config` / `route-test` CLI and `SIGHUP` reload.
6. `docs/DEPLOY.md` — operator surface: admin auth & RBAC, webhook/CDR
   delivery (signing, durability), metrics, and the systemd / reload flow.
7. Feature guides — `docs/OUTBOUND.md`, `docs/CONFERENCE.md`,
   `docs/PARK.md`, `docs/RECORDING.md`, `docs/SECURITY_STIR_SHAKEN.md`,
   `docs/AUDIT.md`, `docs/OPERATIONS.md` (dashboards, OTLP, fleet ops),
   `docs/INTEGRATIONS_TWILIO.md`, `docs/CONFORMANCE.md` (testkit for WS
   server authors).

## Upstream dependencies

SiphonAI is glue. The heavy lifting lives in three companion repos owned by
the same author:

- [`siphon-rs`](https://github.com/thevoiceguy/siphon-rs) — RFC 3261 SIP stack
- [`forge-media`](https://github.com/thevoiceguy/forge-media) — RTP/codecs/SDP/jitter/VAD
- [`hep-rs`](https://github.com/thevoiceguy/hep-rs) — HEP3 codec, transport, `HepSink` trait

## License

Dual-licensed under MIT or Apache-2.0, matching the upstream stack.
