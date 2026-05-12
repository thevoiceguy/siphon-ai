# SiphonAI — Detailed Development Plan

**Status:** v1 (planning)
**Owner:** TBD
**Target v0.1.0 release:** ~7 weeks from kickoff

---

## 1. Product Definition (Locked)

**SiphonAI is a SIP-to-WebSocket media bridge.** It is *not* an AI product.

It accepts inbound voice calls — either from a SIP trunk or as a registered endpoint on a third-party PBX (Cisco CUCM, Asterisk, FreeSWITCH, BroadWorks, etc.) — answers them, and streams the call's audio over a WebSocket to a developer-supplied server. Audio sent back over the same WebSocket is played into the call. The developer's server is where AI (or anything else) lives — SiphonAI is vendor-neutral and never talks to an AI provider.

```
   ┌──────────────┐                           ┌────────────────┐
   │ SIP Trunk    │   SIP / RTP               │                │
   │ or PBX (CUCM,│ ◄────────────────────►    │   SiphonAI     │
   │  Asterisk…)  │                           │                │
   └──────────────┘                           └───────┬────────┘
                                                      │
                                            WebSocket │ (audio + control)
                                                      ▼
                                              ┌───────────────┐
                                              │ Developer's   │
                                              │ server        │
                                              │ (BYO AI:      │
                                              │  OpenAI,      │
                                              │  Deepgram+    │
                                              │  Anthropic+   │
                                              │  ElevenLabs,  │
                                              │  whatever)    │
                                              └───────────────┘
```

**v1 promise:**
> "Point your PBX at SiphonAI, point SiphonAI at your WebSocket. Get real-time bidirectional audio with barge-in, DTMF, and call control. Bring your own AI."

**v1 explicitly does NOT include:**
- AI integration (any kind)
- Multi-tenancy / per-DID routing (single global config)
- Outbound originated calls (inbound-only for v1)
- Conferencing
- Video
- Recording (deferred — forge has it, we'll expose it post-v1)
- WebRTC client support (forge has it, but SiphonAI is SIP-only for v1)

---

## 2. Locked Decisions

| # | Decision | Rationale |
|---|---|---|
| D1 | **New repo** `siphon-ai`, separate from `siphon-rs` and `forge-media` | Clean OSS positioning; siphond stays a test daemon |
| D2 | **forge-media linked as a library** (Rust crate dep), not via REST | Single binary, lower latency, simpler deploy |
| D3 | **siphon-rs linked as a library** | Same |
| D4 | **Single global config** — no multi-tenancy in v1 | User scope: individual users/orgs dropping in front of existing SIP infra |
| D5 | **Bring-your-own AI** — SiphonAI ships zero AI code | Forge's `forge-ai-stream` is **not** used by SiphonAI |
| D6 | Both **SIP trunk mode** (UAS) and **registered phone mode** (UAC + REGISTER) supported in v1 | Both are common deployment shapes; siphon-rs already supports them |
| D7 | **WebSocket protocol:** JSON text frames for control + binary frames for raw PCM audio | No base64 overhead; simple to implement on the server side |
| D8 | **Audio format on the wire:** PCM16 little-endian, 20 ms frames, 8 kHz default | Matches narrowband SIP; configurable to 16 kHz |
| D9 | **One WS connection per call** | Simpler isolation; pool later if needed |
| D10 | **Pin upstream crates by git rev**, not version | siphon-rs and forge-media will move fast; no published crates yet |
| D11 | **HEP/EEP support** for Homer/HEPIC/HEPlify-Server collectors. Implemented as a shared `hep-rs` crate consumed by siphon-rs (SIP), forge-media (RTCP/QoS), and siphon-ai (logs/CDRs/events) | Industry-standard observability; matches Kamailio/OpenSIPS/FreeSWITCH ecosystem |
| D12 | **TOML for configuration**, not YAML/JSON/XML | Rust-native, comments work, `[[route]]` arrays-of-tables fit the dialplan model cleanly, no whitespace footguns |
| D13 | **Route-based dispatch** in a single config file (multiple `[[route]]` entries with match rules → bridge config), evaluated in order with a default fallback | "Single config" doesn't preclude multiple lines/extensions within one tenant — the user just doesn't need multi-*tenancy* |
| D14 | **CDR generation** (JSON, written to disk and/or pushed via webhook) at call end | Standard telecom expectation; cheap to implement; useful for billing/audit |
| D15 | **Lifecycle webhooks** for out-of-band events (call_start, call_end, registration_state_changed) — separate channel from the per-call WS bridge | Some integrations need to know about calls without being the bridge endpoint |

---

## 3. What We Get From Each Upstream

### 3.1 From `siphon-rs` (use directly)

| Crate | Purpose in SiphonAI |
|---|---|
| `sip-core`, `sip-parse` | Message types and parsing |
| `sip-transport` | UDP/TCP/TLS transport |
| `sip-transaction` | RFC 3261 transactions (retransmissions, timers) |
| `sip-dialog` | Dialog state management |
| `sip-uas` | Inbound INVITE handling (trunk mode) |
| `sip-uac` | REGISTER + outbound for registered-phone mode |
| `sip-auth` | Digest auth challenge/response |
| `sip-dns` | Resolving the registrar/PBX hostnames |
| `sip-observe` | Metrics integration |

**HEP/SIP capture (PR upstream):** Add a `sip-hep` crate (or feature flag in `sip-observe`) that consumes a `HepSink` trait from the new `hep-rs` crate and emits HEP3 packets for every parsed inbound and serialized outbound SIP message, with correlation_id derived from Call-ID.

**Gap to fill:** `sip-sdp` is a placeholder in siphon-rs. **Use `forge-sdp` instead** — forge already has a working SDP crate. SiphonAI bridges between them by passing SDP as opaque strings into siphon-rs and parsing/generating with forge-sdp.

### 3.2 From `forge-media` (use directly)

| Crate | Purpose in SiphonAI |
|---|---|
| `forge-core` | Common types |
| `forge-rtp` | RTP/RTCP packet I/O |
| `forge-codecs` | G.711 µ-law/A-law (mandatory), Opus, G.722 (nice-to-have) |
| `forge-resampler` | 8k ↔ 16k for WS server preference |
| `forge-engine` | Session lifecycle, audio routing primitives |
| `forge-sdp` | SDP offer/answer (fills siphon-rs gap) |
| `forge-injection` | Playing inbound-from-WS audio into the call |
| `forge-dtmf` | DTMF detection → WS events |

**HEP/RTCP capture (PR upstream):** Add a `forge-hep` crate (or feature in observability) that consumes the same `HepSink` trait and emits HEP3 packets for RTCP reports (sender/receiver reports, RTCP-XR if available) and periodic RTP QoS summaries (jitter, loss, MOS estimate).

### 3.3 From `forge-media` — explicitly NOT used in v1

- `forge-ai-stream` — SiphonAI is BYO-AI
- `forge-conference-processor`, `forge-mixer` — 1:1 calls only
- `forge-webrtc` — SIP-only
- `forge-siprec`, `forge-recording` — deferred
- `forge-api` — we're using forge as a library, not consuming its REST API
- `forge-ha`, `forge-kernel` — overkill for v1

### 3.4 Critical Week-1 Spike

**The single biggest unknown:** does `forge-engine` expose a clean *bidirectional audio tap* we can hook to ship frames to a WebSocket and inject frames from one?

- forge's existing AI integration must do exactly this internally (it streams audio to/from OpenAI's WS)
- Look at how `forge-ai-stream` plugs into `forge-engine` — that's the integration point we want to use
- If the tap is a public trait/API: use it directly
- If it's internal: open a small PR upstream to extract it as a public trait (e.g., `MediaTap` or `AudioStreamConsumer`), then SiphonAI implements that trait pointing at our WS

**This is Day 1 work.** The shape of the tap determines the shape of `siphon-ai-core`.

### 3.5 HEP/EEP Architecture (cross-cutting)

The HEP3 protocol (Homer Encapsulation Protocol v3) is the standard way to ship SIP signaling, RTCP, RTP QoS, logs, and CDRs to a central capture server (Homer, HEPIC, HEPlify-Server). Every serious VoIP platform in the OSS world supports it: FreeSWITCH (`mod_sofia` HEP), Kamailio (`siptrace`), OpenSIPS (`proto_hep`), Asterisk (`res_hep`), rtpengine. **SiphonAI must support it natively**, not as an afterthought.

**Topology:**

```
                  ┌──────────────────────────────────────┐
                  │           hep-rs (new crate)         │
                  │  HEP3 codec, transport, HepSink trait│
                  └──────────────────────────────────────┘
                         ▲                ▲           ▲
                         │                │           │
              ┌──────────┴────┐  ┌────────┴──────┐  ┌─┴────────────┐
              │   siphon-rs   │  │  forge-media  │  │  siphon-ai   │
              │  (SIP msgs)   │  │ (RTCP, QoS)   │  │ (logs, CDRs, │
              │               │  │               │  │   events)    │
              └───────────────┘  └───────────────┘  └──────────────┘
                         │                │           │
                         └────────────────┼───────────┘
                                          ▼
                                  ┌───────────────┐
                                  │ Homer/HEPIC/  │
                                  │ HEPlify-Server│
                                  │  (UDP/TCP/TLS)│
                                  └───────────────┘
```

**Key design choices:**

- **`hep-rs` is a new standalone crate**, owned by us, MIT/Apache-2.0 dual-licensed. Reusable by anyone in the Rust VoIP world. Lives in its own repo.
- **`HepSink` trait** is the integration point. siphon-rs and forge-media accept an `Option<Arc<dyn HepSink>>` at initialization; when `Some`, they emit; when `None`, zero overhead.
- **All HEP emission is non-blocking.** The sink batches (configurable, default 10 packets or 50ms) and ships to a worker task. If the collector is unreachable, packets drop with a metric increment — never block the audio path.
- **Correlation:** SiphonAI's internal `call_id` is included as HEP chunk 0x0011 (correlation ID) in every packet, so Homer can stitch SIP + RTCP + logs into one call view.
- **Capture ID and password** are SiphonAI config. siphon-rs and forge-media don't see them — they just call `sink.send_*()` and the sink handles auth/encoding.

**HEP chunk types we emit:**

| Type | From | What |
|---|---|---|
| 0x01 (SIP) | siphon-rs | Every inbound/outbound SIP message |
| 0x05 (RTCP) | forge-media | SR/RR/SDES/BYE; RTCP-XR if present |
| 0x63 (vendor: RTP QoS) | forge-media | Periodic jitter/loss/MOS per stream |
| 0x64 (log) | siphon-ai | Significant lifecycle events as text |
| 0x65 (CDR) | siphon-ai | Call detail records at end-of-call |

**Required upstream PRs:**

1. siphon-rs: `sip-hep` crate (~1 week, mostly straightforward — wrap parse/serialize hooks)
2. forge-media: `forge-hep` crate (~1 week — RTCP capture is straightforward, QoS reporting is more involved)

Both PRs land before SiphonAI Week 5 ships HEP integration end-to-end. Coordinate with the upstream maintainers (i.e., yourself) early in Week 1 so the work parallelizes.

**Why not just packet-mirror?** Capture agents like `heplify` or `captagent` work by sniffing packets off the wire. That works but loses internal context (parsed dialog state, decoded codecs, application-level events). Native HEP emission lets us send richer correlation data and works in containers/cloud where promiscuous packet capture isn't available.

---

## 4. WebSocket Bridge Protocol — v1 Spec

This is the public API of SiphonAI. Treat it like a contract; version it from day one.

### 4.1 Connection

- One WS connection per call
- SiphonAI is the WS **client**; the developer runs the WS **server**
- URL configured globally in `siphon-ai.yaml` (`bridge.ws_url`)
- Optional bearer token in `Authorization` header
- TLS (`wss://`) supported and recommended

### 4.2 Frame Types

- **Text frames:** JSON control messages (both directions)
- **Binary frames:** raw audio (both directions)
  - PCM16 little-endian, mono
  - 20 ms per frame: 160 samples @ 8 kHz, 320 samples @ 16 kHz
  - No header — preceded by a control message that establishes the format

### 4.3 SiphonAI → Server messages

```json
// Sent immediately on connect, before any audio
{
  "type": "start",
  "version": "1",
  "call_id": "siphon-7f3a9b21",
  "seq": 0,
  "from": "+13125551212",
  "to": "5000",
  "direction": "inbound",
  "audio": { "encoding": "pcm16le", "sample_rate": 8000, "channels": 1, "frame_ms": 20 },
  "sip": {
    "call_id": "...@pbx.example.com",
    "headers": { "User-Agent": "Cisco-CP8841", "P-Asserted-Identity": "..." }
  }
}

// Speech detection (if VAD enabled)
{ "type": "speech_started", "call_id": "...", "seq": 42, "ts_ms": 1234 }
{ "type": "speech_stopped", "call_id": "...", "seq": 67, "ts_ms": 1890, "duration_ms": 656 }

// DTMF
{ "type": "dtmf", "call_id": "...", "seq": 88, "digit": "5", "duration_ms": 120 }

// Mark fired (server-requested playback marker — see below)
{ "type": "mark", "call_id": "...", "seq": 91, "name": "greeting_done" }

// Call ended
{ "type": "stop", "call_id": "...", "seq": 200, "reason": "caller_hangup" }
//   reason ∈ { "caller_hangup", "server_hangup", "transfer", "ws_disconnect", "error" }

// Errors
{ "type": "error", "call_id": "...", "seq": 201, "code": "rtp_timeout", "message": "..." }
```

Plus binary audio frames interleaved with these, at 50 frames/sec for 20 ms.

### 4.4 Server → SiphonAI messages

```json
// Drop pending outbound playback (barge-in handling)
{ "type": "clear", "call_id": "..." }

// Insert a marker into the playback stream — SiphonAI fires "mark" when audio up to this point has played
{ "type": "mark", "call_id": "...", "name": "greeting_done" }

// Hang up the call
{ "type": "hangup", "call_id": "...", "cause": "normal" }

// Initiate blind transfer (REFER)
{ "type": "transfer", "call_id": "...", "target": "sip:agent@example.com" }

// Send DTMF toward the caller (RFC2833)
{ "type": "send_dtmf", "call_id": "...", "digit": "1", "duration_ms": 200 }
```

Plus binary audio frames. Same format as inbound.

### 4.5 Protocol Rules

- `seq` increments monotonically on SiphonAI→server messages (debug aid, not flow control)
- `call_id` is SiphonAI's internal call ID, not the SIP Call-ID (which is in `sip.call_id` on `start`)
- Server must accept `start` and begin sending audio within 5 s or SiphonAI tears down with `error`
- If WS disconnects mid-call: SiphonAI plays a configured "we're having trouble" prompt (or silence) and ends the call
- If WS reconnects (for the same call_id) within 2 s: resume (best-effort, post-v1)
- Audio backpressure: SiphonAI buffers max 200 ms of outbound audio; beyond that, drops oldest

### 4.6 Versioning

- Bump `version` field on `start` message for breaking changes
- Server can negotiate by checking `version` and disconnecting if unsupported

---

## 5. Workspace Layout

```
siphon-ai/
├── Cargo.toml                         # workspace
├── README.md
├── docker/
│   ├── Dockerfile
│   └── compose.yaml                   # siphon-ai + siphond-as-fake-PBX + echo-ws-server + Homer
├── docs/
│   ├── PROTOCOL.md                    # WS protocol v1 spec (canonical)
│   ├── DEPLOY.md
│   ├── CONFIG.md                      # Config reference (TOML schema, every field documented)
│   ├── DIALPLAN.md                    # Route matching semantics with examples
│   ├── HEP.md                         # HEP integration setup and Homer correlation
│   ├── REGISTRATION.md                # how to register to CUCM/Asterisk
│   └── EXAMPLES.md
├── crates/
│   ├── core/                          # CallController, state machine, glue
│   ├── bridge/                        # WS client + protocol types + audio bridging
│   ├── sip-glue/                      # Adapter from siphon-rs UAS/UAC events to core
│   ├── media-glue/                    # Adapter from forge-engine to core (the "tap")
│   ├── routes/                        # Route matching engine (request-URI/header/AOR → route)
│   ├── cdr/                           # CDR generation (JSON), file sink, webhook sink
│   ├── webhooks/                      # Out-of-band lifecycle webhooks (HTTP POST)
│   ├── config/                        # TOML config + validation + reload
│   └── telemetry/                     # tracing + metrics + HEP wiring
├── bins/
│   └── siphon-ai/                     # The daemon
├── examples/
│   ├── echo-ws-server-python/         # Reference: WS server that echoes audio back
│   ├── echo-ws-server-node/           # Same in Node
│   ├── openai-realtime-bridge-py/     # Reference: WS server that bridges to OpenAI Realtime
│   └── homer-stack/                   # docker-compose for local Homer + dashboards
└── test-harness/
    ├── sipp-scenarios/                # SIPp scripts mirroring siphon-rs's pattern
    ├── load/                          # k6 / custom load tooling
    ├── hep-collector-stub/            # Tiny HEP3 receiver for tests
    └── interop/                       # Asterisk + CUCM lab notes/scripts
```

External dependencies (separate repos, owned by us):
- **`hep-rs`** (new) — HEP3 codec, transport, `HepSink` trait
- **`siphon-rs`** — SIP stack (with `sip-hep` PR landed)
- **`forge-media`** — Media engine (with `forge-hep` PR landed)

### 5.1 Core Module Sketches

```rust
// crates/core/src/call.rs

pub struct CallController {
    call_id: CallId,
    sip: SipDialog,            // from sip-glue
    media: MediaSession,       // from media-glue (wraps forge-engine session)
    bridge: BridgeConn,        // from bridge crate (WS connection)
    state: CallState,
    cfg: Arc<Config>,
}

pub enum CallState {
    Initializing,
    Answering,
    Active,
    OnHold,
    Transferring,
    Terminating,
    Done,
}

impl CallController {
    pub async fn run(mut self) -> Result<()> {
        // 1. Open WS connection to bridge.ws_url
        // 2. Send `start` event with call metadata
        // 3. Spawn three tasks:
        //    a. media_in_task:   forge audio frames → bridge.send_audio()
        //    b. media_out_task:  bridge.recv_audio() → forge injection
        //    c. control_task:    bridge.recv_control() → handle (clear, mark, hangup, transfer, send_dtmf)
        //    d. sip_event_task:  siphon-rs dialog events → state transitions
        //    e. dtmf_task:       forge-dtmf events → bridge.send_event(dtmf)
        // 4. Await termination signal from any source
        // 5. Tear down: send `stop` event, close WS, hangup SIP if needed, release media
    }
}
```

```rust
// crates/bridge/src/protocol.rs

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BridgeOut {  // SiphonAI -> Server
    Start(StartMsg),
    SpeechStarted { call_id: String, seq: u64, ts_ms: u64 },
    SpeechStopped { call_id: String, seq: u64, ts_ms: u64, duration_ms: u64 },
    Dtmf { call_id: String, seq: u64, digit: char, duration_ms: u32 },
    Mark { call_id: String, seq: u64, name: String },
    Stop { call_id: String, seq: u64, reason: StopReason },
    Error { call_id: String, seq: u64, code: String, message: String },
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BridgeIn {  // Server -> SiphonAI
    Clear { call_id: String },
    Mark { call_id: String, name: String },
    Hangup { call_id: String, cause: Option<String> },
    Transfer { call_id: String, target: String },
    SendDtmf { call_id: String, digit: char, duration_ms: u32 },
}
```

```rust
// crates/media-glue/src/tap.rs
// This is the spike target in Week 1 — exact shape depends on what forge-engine exposes

pub trait MediaTap: Send {
    /// Called every 20ms with PCM16 audio from the call (caller → us)
    fn on_inbound_frame(&mut self, pcm: &[i16]);
    /// Pull next frame to play out into the call (us → caller); None = silence
    fn next_outbound_frame(&mut self) -> Option<Vec<i16>>;
    /// Notify of out-of-band events
    fn on_event(&mut self, event: MediaEvent);
}

pub enum MediaEvent {
    Dtmf { digit: char, duration_ms: u32 },
    SpeechStarted,
    SpeechStopped { duration_ms: u32 },
    RtpTimeout,
}
```

---

## 6. Configuration (v1)

Single TOML file. Reloadable on `SIGHUP`. Schema validated at load time.

### 6.1 Why TOML

- Rust-native (Cargo uses it; everyone already knows the syntax)
- Comments work
- No whitespace-significance footguns
- `[[route]]` arrays-of-tables fit the dialplan model perfectly
- Scalar override via env vars works cleanly (`${VAR}` syntax)

### 6.2 Top-Level Structure

```toml
# siphon-ai.toml

# ─── Identity ────────────────────────────────────────────────────────────
[node]
id = "siphon-ai-01"           # used in logs, metrics, HEP capture_id
public_address = "1.2.3.4"    # for SDP when behind NAT (optional)

# ─── SIP transport ───────────────────────────────────────────────────────
[sip]
listen = "0.0.0.0:5060"
transports = ["udp", "tcp"]
user_agent = "SiphonAI/0.1.0"

# Optional: TLS
[sip.tls]
enabled = false
listen = "0.0.0.0:5061"
cert = "/etc/siphon-ai/tls/cert.pem"
key  = "/etc/siphon-ai/tls/key.pem"

# ─── PBX registrations (optional, for "registered phone" mode) ───────────
# Zero or more; each registration becomes available as a route source.
[[register]]
name = "cucm-main"
server = "cucm.example.com"
port = 5060
transport = "tcp"
username = "ai-receptionist"
auth_username = "ai-receptionist"
password = "${SIP_PASSWORD_CUCM}"
realm = "example.com"
expires_secs = 3600
register_on_startup = true

[[register]]
name = "asterisk-sales"
server = "asterisk.example.com"
username = "sales-bot"
password = "${SIP_PASSWORD_ASTERISK}"
expires_secs = 1800

# ─── Media defaults (per-route can override) ─────────────────────────────
[media]
rtp_port_range = [16384, 32768]
codecs = ["pcmu", "pcma"]            # offered priority. Opus is post-v1 (needs 48k→16k resampling in forge).
dtmf = "rfc2833"                     # rfc2833 | info | both
inactivity_timeout_secs = 60

[media.vad]
enabled = true
aggressiveness = 2                   # 0-3
speech_pad_ms = 50

# ─── Bridge defaults (per-route can override) ────────────────────────────
# Audio sample rate on the WS path is determined by the negotiated codec
# (PCMU/PCMA = 8 kHz, G.722 = 16 kHz). It is NOT a configurable knob — see
# CLAUDE.md §4.2 and the v1 protocol's fixed PCM16/8k|16k contract.
[bridge]
audio_direction = "bidirectional"    # bidirectional | inbound_only
on_ws_failure = "hangup"             # hangup | play_prompt
on_ws_failure_prompt = "/etc/siphon-ai/prompts/trouble.wav"
ws_reconnect_enabled = false         # post-v1
ws_connect_timeout_ms = 3000

[bridge.barge_in]
enabled = true
mode = "auto_clear"                  # auto_clear | notify_only
debounce_ms = 100

# ─── Routes (the dialplan) ───────────────────────────────────────────────
# Evaluated TOP-DOWN. First match wins. A trailing default route is recommended.
# Matching is on the inbound INVITE: request_uri, To, From, Call-ID, custom headers.

[[route]]
name = "main_reception"
[route.match]
request_uri_user = "5000"            # exact match on user part of Request-URI
[route.bridge]
ws_url = "wss://reception.example.com/sip-bridge"
ws_auth_header = "Bearer ${BRIDGE_TOKEN_RECEPTION}"

[[route]]
name = "sales_team"
[route.match]
request_uri_user = "^sales-[0-9]+$"  # regex (when `regex = true`)
regex = true
[route.bridge]
ws_url = "wss://sales.example.com/sip-bridge"
ws_auth_header = "Bearer ${BRIDGE_TOKEN_SALES}"

[[route]]
name = "vip_caller_id"
[route.match]
from_user = "+13125551234"
[route.bridge]
ws_url = "wss://vip.example.com/sip-bridge"
ws_auth_header = "Bearer ${BRIDGE_TOKEN_VIP}"

[[route]]
name = "from_cucm_only"
[route.match]
register_source = "cucm-main"        # call arrived via this registration
[route.bridge]
ws_url = "wss://cucm-handler.example.com/sip-bridge"

[[route]]
name = "header_match"
[route.match]
header.X-Customer-Id = "^cust-.*$"   # regex on a custom header
regex = true
[route.bridge]
ws_url = "wss://customer-handler.example.com/sip-bridge"

# Default fallback — must be last. Matches everything.
[[route]]
name = "default"
[route.match]
any = true
[route.bridge]
ws_url = "wss://default.example.com/sip-bridge"
ws_auth_header = "Bearer ${BRIDGE_TOKEN_DEFAULT}"

# ─── HEP / Homer ─────────────────────────────────────────────────────────
[hep]
enabled = true
collector = "homer.example.com:9060"
transport = "udp"                    # udp | tcp | tls
capture_id = 2001                    # Homer agent ID
capture_password = "${HEP_PASSWORD}"
emit_sip = true
emit_rtcp = true
emit_rtp_qos = true                  # periodic per-stream QoS
emit_logs = false                    # log emission is heavy; opt-in
emit_cdr = true
batch_max_packets = 10
batch_max_age_ms = 50

# ─── CDR (Call Detail Records) ───────────────────────────────────────────
[cdr]
enabled = true
format = "json"                      # json | csv (csv post-v1)

[cdr.file]
enabled = true
path = "/var/log/siphon-ai/cdr.jsonl"
rotate = "daily"                     # daily | size:100MB | none

[cdr.webhook]
enabled = false
url = "https://billing.example.com/cdr"
auth_header = "Bearer ${CDR_WEBHOOK_TOKEN}"
retry_max = 3
timeout_ms = 5000

# ─── Lifecycle webhooks (out-of-band events; not the per-call WS) ────────
[webhooks]
enabled = false
url = "https://ops.example.com/siphon-events"
auth_header = "Bearer ${WEBHOOK_TOKEN}"
events = ["call_start", "call_end", "registration_state_changed", "ws_failure"]
retry_max = 3
timeout_ms = 5000

# ─── Observability ───────────────────────────────────────────────────────
[observability]
log_level = "info"                   # error | warn | info | debug | trace
log_format = "json"                  # json | text
log_targets = "siphon_ai=debug,siphon=info,forge=info,tower_http=warn"
metrics_listen = "0.0.0.0:9090"
health_listen = "0.0.0.0:9091"       # /health (liveness), /ready (readiness)
admin_listen = "127.0.0.1:9092"      # admin API: dynamic log-level adjustment
trace_endpoint = ""                  # OTLP gRPC endpoint (e.g., http://otel:4317), empty = disabled
trace_sample_ratio = 1.0             # 0.0-1.0; downsample for high call volumes

# ─── Security ────────────────────────────────────────────────────────────
[security]
sip_acl_allow = ["10.0.0.0/8", "192.168.0.0/16"]
max_concurrent_calls = 500
max_calls_per_second = 50
require_register_auth = true         # reject unauthenticated REGISTERs
```

### 6.3 Route Matching Semantics

- **Order matters.** Routes are evaluated top-down; first match wins.
- **Match keys:** `request_uri_user`, `request_uri_host`, `to_user`, `to_host`, `from_user`, `from_host`, `register_source` (which `[[register]]` block this call arrived via, or `"trunk"` for unregistered inbound), `header.<NAME>` for arbitrary header matching, `any = true` for unconditional match.
- **Modes:** by default, exact case-insensitive string match. Set `regex = true` to interpret all string match values in this route as Rust regex patterns.
- **Multiple match keys are AND'd.** All must match.
- **Per-route override:** `[route.bridge]` and `[route.media]` blocks override the global defaults for matched calls. Anything not specified inherits from the global block.
- **No match:** if no route matches and there's no `any = true` default, the call is rejected with SIP 404. Log a warning at startup if no default route is configured.

Full matching grammar and examples in `docs/DIALPLAN.md`.

### 6.4 Reload

- `SIGHUP` triggers config reload
- New config validated before swap; on validation failure, the old config stays active and an error is logged
- Active calls keep the route bridge config they started with — no mid-call config changes
- Registration changes (added/removed/modified `[[register]]`) re-register or de-register as appropriate

### 6.5 Validation

At load:
- Every `[[route]]` parses cleanly
- All referenced `register_source` names exist in `[[register]]` blocks
- All regex patterns compile
- All file paths (TLS certs, prompts, CDR file directory) exist or are creatable
- All env vars referenced in `${VAR}` syntax are set
- TLS cert/key load successfully
- Warn if no default route exists

### 6.6 Env Var Expansion

`${VAR}` anywhere in a string value is replaced at load time. Supports `${VAR:-default}` for optional vars with defaults. Never logged in resolved form.

---

## 7. SIP Modes — How Each Works

### 7.1 Trunk Mode (UAS)

1. SiphonAI binds `sip.listen` and waits for INVITEs
2. Inbound INVITE → siphon-rs sip-uas hands it to SiphonAI
3. SiphonAI generates SDP answer via forge-sdp pointing at a forge-allocated RTP port
4. Sends 100 Trying → 200 OK with SDP → awaits ACK
5. Once ACK received, opens WS, starts streaming
6. BYE: tear down WS + media

**Test target:** Asterisk PJSIP trunk pointing at SiphonAI.

### 7.2 Registered Mode (UAC + REGISTER)

1. On startup, SiphonAI sends REGISTER to `sip.register.server` with credentials
2. Handles 401 challenge using sip-auth
3. Refreshes registration at `expires_secs / 2`
4. PBX routes a call to our registered AOR → INVITE arrives at SiphonAI
5. Same flow as trunk mode from there

**Test target:** Cisco CUCM (primary), Asterisk (secondary), FreeSWITCH (nice-to-have).

### 7.3 NAT Considerations (v1: minimal)

- `sip.public_address` overrides `c=` in SDP for environments behind NAT
- Symmetric RTP: forge-engine should already do this (verify in spike)
- Full ICE/STUN: deferred to post-v1 (forge has WebRTC ICE; SIP-side STUN is a bigger lift)

---

## 8. Performance & Latency Budget

For a typical call: caller speaks → server's AI responds → caller hears it.

| Stage | Budget | Notes |
|---|---|---|
| Network: caller → SiphonAI | 30 ms | Within carrier |
| Forge jitter buffer | 60 ms | Default; tune later |
| Forge decode (G.711→PCM16) | <1 ms | Table lookup |
| SiphonAI tap → WS frame | <1 ms | Same process, channel send |
| WS network: SiphonAI → server | 10–30 ms | LAN/WAN dependent |
| **Server-side AI (everything)** | **150–500 ms** | Out of scope; this is the developer's problem |
| WS network: server → SiphonAI | 10–30 ms | |
| Forge playout buffer | 60 ms | |
| Forge encode (PCM16→G.711) | <1 ms | |
| Network: SiphonAI → caller | 30 ms | |
| **Total round-trip first audio** | **~350–750 ms** | |

**SiphonAI's contribution to latency: ~165 ms.** That's the budget the bridge owns. Anything beyond that is in the WS server or the network.

**Performance targets (per node):**
- 500 concurrent calls @ G.711 on a 4-core box (forge claims 1000+ — derate for SiphonAI overhead)
- p99 added latency from SiphonAI bridge: <20 ms (excluding network, jitter buffer, playout)
- 50 calls/sec setup rate

---

## 9. Sprint Plan (7 Weeks)

### Week 1 — Foundation + Forge Tap Spike

**Goals:**
- Workspace, CI green, dependencies pinned
- One inbound call answered end-to-end with **echo-only** (no WS yet)

**Tasks:**
1. Create repo, workspace `Cargo.toml`, GitHub Actions (fmt, clippy, test, audit)
2. Pin siphon-rs and forge-media as git deps
3. **SPIKE: forge-engine bidirectional tap**. Find or build the integration point. *This is the highest-risk Week-1 task.* Outcomes:
   - Document `MediaTap` trait or equivalent
   - If trait must be added upstream: open PR to forge-media
4. Implement `sip-glue` for trunk-mode INVITE → 200 OK using siphon-rs sip-uas
5. Use forge-sdp to generate the SDP answer pointing at a forge-allocated RTP port
6. Wire forge-engine to echo received audio back (no WS yet — pure media-loopback test)
7. BYE handling, clean teardown
8. Smoke test with linphone-cli or pjsua

**Acceptance:** Softphone calls SiphonAI, gets answered, hears their own voice echoed back, BYE works cleanly. 5-minute call stable.

**Risk to flag:** if the forge tap spike requires significant upstream changes, slip Week 2 by a few days.

---

### Week 2 — WebSocket Bridge + Route Matching

**Goals:**
- WS protocol v1 implemented
- Bidirectional audio between SIP call and a developer's WS server
- Route matching engine: incoming INVITE → matched route → bridge config

**Tasks:**
1. Implement `bridge` crate: `tokio-tungstenite` client, protocol types (`BridgeIn`/`BridgeOut`), connection lifecycle
2. Implement `start` event with full call metadata (including `traceparent`)
3. Wire `MediaTap`: inbound forge frames → WS binary; WS binary → forge-injection
4. (Reserved.) WS audio rate is fixed by the negotiated codec in v1; resampling for codecs whose audio rate is not 8 kHz / 16 kHz (e.g., Opus 48 kHz) is post-v1 work in `forge-resampler`.
5. Implement `routes` crate: TOML loading, match evaluation (string/regex/header/register_source), per-route override merging
6. Wire route lookup into the call flow: INVITE → match → use route's bridge config (not global)
7. Handle `clear` (drop pending playback buffer)
8. Handle `hangup`
9. Build Python and Node echo-WS-server examples
10. Document protocol in `docs/PROTOCOL.md`
11. Document dialplan in `docs/DIALPLAN.md`

**Acceptance:**
- SIP call → SiphonAI → echo WS server (Python) → audio comes back into call
- Three different routes configured; calls to different request-URI users hit different WS servers
- A call to an unmatched URI gets SIP 404 (when no default route)
- Latency added by SiphonAI itself (excluding jitter/playout buffers): <20 ms p99
- Protocol doc complete enough that a third party could write a server against it

---

### Week 3 — Call Control & Barge-In

**Goals:**
- Barge-in works
- DTMF flows both ways
- Hold/resume works

**Tasks:**
1. VAD integration (forge has it; wire to emit `speech_started`/`speech_stopped`)
2. `barge_in.mode: auto_clear`: VAD speech-start → drop outbound playback buffer + send `clear`-equivalent event
3. `barge_in.mode: notify_only`: just send `speech_started`, server can decide to `clear`
4. DTMF in: forge-dtmf RFC2833 detection → WS `dtmf` event
5. DTMF out: WS `send_dtmf` → forge generates RFC2833
6. Hold (re-INVITE with `a=sendonly` or `a=inactive`): pause WS audio flow (notify server via state event)
7. Resume (re-INVITE back to `sendrecv`): resume audio flow
8. Mark events: implement `mark` round-trip (server inserts marker, fires when played)

**Acceptance:**
- Manual test: caller hears greeting, interrupts, greeting stops within 100 ms
- Pressing 5 on the keypad emits `dtmf` event with `digit: "5"`
- Server sending `send_dtmf: 1` causes caller to hear DTMF tone
- Hold via softphone causes WS audio to pause; resume restores it

---

### Week 4 — Registration Mode

**Goals:**
- SiphonAI can register to a PBX as a phone

**Tasks:**
1. UAC REGISTER flow using siphon-rs sip-uac + sip-auth
2. Handle 401/407 challenges with digest auth
3. Re-registration timer (refresh at `expires/2`)
4. Recovery on registration failure (exponential backoff, configurable max)
5. Test: register to dockerized Asterisk; call from softphone registered to same Asterisk; verify SiphonAI receives the INVITE
6. Test: register to lab Cisco CUCM (if available); same call test
7. Document registration setup in `docs/REGISTRATION.md`

**Acceptance:**
- SiphonAI registers to Asterisk on startup, REGISTER refresh works
- Inbound call to the registered AOR routes to SiphonAI and bridges to WS
- CUCM registration validated (with vendor-specific gotchas documented)

---

### Week 5 — Stability, Transfer, Hardening

**Goals:**
- Calls don't fall over
- REFER (transfer) works
- Resource cleanup is bulletproof

**Tasks:**
1. REFER (blind transfer) handling: server sends `{type: "transfer", target: "sip:..."}` → SiphonAI initiates REFER → caller is transferred → SiphonAI cleans up
2. Mid-call codec robustness (re-INVITE with codec change)
3. Long-call test: 1-hour call, monitor for memory growth, RTP drift, jitter buffer health
4. Burst test: 100 simultaneous call setups in 5 seconds
5. Failure-mode tests:
   - WS server unreachable on call answer → `on_ws_failure` policy works
   - WS disconnects mid-call → call ends cleanly with appropriate `stop` reason
   - Caller hangs up while WS is mid-message → no panics, clean teardown
   - RTP stops flowing (network issue) → `inactivity_timeout_secs` triggers, hangup
6. Memory profiling with heaptrack or `dhat`; fix any leaks
7. Concurrency stress test with SIPp

**Acceptance:**
- 1-hour call: stable, no memory growth, audio quality intact
- 500 concurrent calls sustained for 10 minutes on a reference node
- Transfer flow tested manually + in CI
- All identified failure modes recover without leaking calls

---

### Week 6 — HEP, CDRs, Webhooks, Admin

**Goals:**
- HEP/EEP shipping to Homer end-to-end
- CDR generation
- Lifecycle webhooks
- Admin endpoints + dynamic log levels

**Prerequisite:** `hep-rs` crate built (parallel work in Weeks 4–5), `sip-hep` PR landed in siphon-rs, `forge-hep` PR landed in forge-media. **Coordinate this in Week 1.**

**Tasks:**
1. Build `hep-rs` (or finalize if started earlier): HEP3 codec, UDP/TCP/TLS transport, `HepSink` trait, batching/queueing worker
2. Wire `HepSink` from `siphon-ai/telemetry` into siphon-rs (sip messages) and forge-media (RTCP, QoS)
3. Implement SiphonAI's own HEP emission for logs and CDRs (chunk types 0x64, 0x65)
4. Stand up `examples/homer-stack/` — local Homer + dashboards via docker-compose
5. End-to-end test: place a call, see SIP flow + RTCP + correlated logs in Homer UI
6. Implement `cdr` crate: schema, JSON serialization, file sink (with rotation), webhook sink (with retry)
7. Implement `webhooks` crate: HTTP POST with retry, event filtering, bearer auth
8. Implement health/ready endpoints
9. Implement admin API: log-level adjustment, calls listing, force-hangup, registrations status, HEP test
10. Add cardinality-safe metric labels (route, register_source, codec, etc.)
11. Validate the §11.8 "10 questions" can be answered from logs+traces+HEP for a real call

**Acceptance:**
- A call placed against the demo stack appears in Homer UI with full SIP flow + RTCP + logs correlated by call_id
- CDR file accumulates entries; webhook delivery works; both can run simultaneously
- `/health` and `/ready` respond correctly during startup, steady state, and shutdown
- `POST /admin/log-level` flips a target to debug for 60s and back automatically
- `siphon_ai_hep_collector_up` flips to 0 when Homer is killed and back to 1 when it returns
- All §11.8 diagnostic questions answerable on a recorded test call

---

### Week 7 — Packaging, Docs, Launch

**Goals:**
- Anyone can `docker compose up` and have a working demo in 5 minutes
- v0.1.0 cut

**Tasks:**
1. `Dockerfile` (multi-stage, Debian-slim, ~50 MB image)
2. `docker/compose.yaml`: SiphonAI + siphond-as-fake-PBX + Python echo WS server
3. README with copy-paste demo
4. `examples/openai-realtime-bridge-py/` — a working WS server that bridges to OpenAI Realtime as the canonical reference (lots of users will want this)
5. Architecture diagram (use mermaid in README)
6. Quickstart for both modes (trunk and register)
7. Troubleshooting guide
8. Tag v0.1.0, GitHub release with binaries for Linux x86_64
9. Announce: HN, /r/voip, /r/rust, telecom Slack/Discord communities

**Acceptance:**
- Cold-start a fresh box: `git clone && docker compose up` → SIPp scenario completes successfully → audio loopback works → done in <5 minutes

---

## 10. Test Strategy

### 10.1 Unit Tests (per crate)
- `bridge`: protocol serialization round-trips, framing edge cases
- `core`: state machine transitions, all paths to terminal states
- `sip-glue`: dialog event mapping
- `media-glue`: tap-trait conformance with a mock `MediaTap`

### 10.2 Integration Tests
- **In-process:** spin up SiphonAI in a test harness with mocks for siphon-rs UAS and a stub WS server; test full call flows
- **SIPp scenarios:** mirror siphon-rs's `sip-testkit/sipp/` pattern. Scenarios:
  - basic_call_then_bye
  - hold_resume
  - dtmf_inband
  - dtmf_info
  - blind_transfer
  - re_invite_codec_change
  - caller_hangup_during_playback
  - pbx_initiated_disconnect
- **Real PBX interop:** dockerized Asterisk in CI; full INVITE/REGISTER/transfer flows

### 10.3 Audio Quality
- Loopback PESQ/POLQA test (or simpler: bit-exact loopback through forge codecs at 8k and 16k)
- Continuous-tone test: 1 kHz tone for 60 s, measure SNR at the WS server side

### 10.4 Load Testing
- SIPp `-r` for call setup rate; ramp to 50 cps, hold for 5 minutes
- Long-running soak: 100 concurrent calls for 24 hours

### 10.5 What We Inherit (Don't Redo)
- siphon-rs has 1000+ tests for SIP correctness — trust them
- forge-media has its own benchmarks — trust them
- SiphonAI's tests should focus on the **bridge logic** and **integration**

---

## 11. Observability

Observability is a v1 requirement, not a post-launch nice-to-have. The system is opaque if you can't tell what's happening per-call, per-route, and per-registration. Five pillars, all wired in by Week 6:

1. **Structured logs** — every event tied to call_id, route, node
2. **Metrics** — Prometheus, with labels for route/codec/registration
3. **Distributed tracing** — OpenTelemetry, one root span per call
4. **HEP/EEP** — SIP, RTCP, QoS, logs, CDRs to Homer
5. **Out-of-band webhooks** — async notifications for ops integrations

Plus CDRs for billing/audit and health/admin endpoints for ops.

### 11.1 Structured Logs

- **Format:** JSON by default; switchable to text via `observability.log_format`
- **Fields on every line:** `timestamp`, `level`, `target`, `node_id`, `message`. Plus span fields when in a span: `call_id`, `route`, `register_source`, `sip_call_id`, `from`, `to`, `direction`, etc.
- **Levels:**
  - `error`: call failed, system-level problem (collector unreachable, registration auth failed)
  - `warn`: recoverable degraded behavior (HEP packet dropped, WS reconnect)
  - `info`: significant lifecycle events (call answered, transferred, ended; registration up/down)
  - `debug`: dialog events, state transitions, SDP negotiation outcomes
  - `trace`: per-frame audio routing (off by default; never enable in prod — too chatty)
- **Per-target levels** via `RUST_LOG`-style syntax in config: `siphon_ai=debug,siphon=info,forge=info`
- **Dynamic adjustment** at runtime via the admin endpoint (see §11.7) — bump a specific target to `debug` for 10 minutes without restart, then it auto-reverts
- **No PII in logs by default** — phone numbers and SIP URIs are hashed when `observability.redact_pii = true`. Off by default; on for prod.
- **Log rotation:** delegated to systemd/logrotate/journald; we write to stdout

### 11.2 Metrics (Prometheus)

Exposed on `observability.metrics_listen`. All metrics prefixed `siphon_ai_`.

**Call lifecycle:**
```
siphon_ai_calls_active{route, register_source}
siphon_ai_calls_total{route, register_source, outcome}
  # outcome ∈ {answered, completed, failed_no_route, failed_no_ws, failed_sip,
  #            failed_codec, transferred, caller_hangup, server_hangup, timeout}
siphon_ai_call_setup_duration_seconds (histogram, labels: route)
siphon_ai_call_duration_seconds (histogram, labels: route)
siphon_ai_call_answer_latency_seconds (histogram)  # INVITE→200 OK
```

**SIP:**
```
siphon_ai_sip_messages_total{direction, method, transport}
siphon_ai_sip_responses_total{class}  # 1xx, 2xx, 3xx, 4xx, 5xx, 6xx
siphon_ai_sip_response_codes_total{code}  # specific codes (404, 486, 503...)
siphon_ai_sip_retransmissions_total{method}
siphon_ai_sip_dialog_active
```

**Registration:**
```
siphon_ai_register_state{name, state}  # state ∈ {registered, registering, failed, expired}
siphon_ai_register_attempts_total{name, outcome}
siphon_ai_register_response_seconds (histogram, labels: name)
```

**Media:**
```
siphon_ai_media_sessions_active
siphon_ai_audio_frames_in_total{codec}
siphon_ai_audio_frames_out_total{codec}
siphon_ai_audio_underruns_total{call_id_hash}
siphon_ai_audio_overruns_total{call_id_hash}
siphon_ai_jitter_ms (histogram)
siphon_ai_packet_loss_ratio (histogram)
siphon_ai_codec_negotiated_total{codec}
siphon_ai_dtmf_events_total{direction}
siphon_ai_barge_in_events_total{route}
```

**WebSocket bridge:**
```
siphon_ai_ws_connections_active
siphon_ai_ws_connect_duration_seconds (histogram, labels: route)
siphon_ai_ws_disconnects_total{route, reason}
siphon_ai_ws_messages_total{direction, type}
siphon_ai_ws_audio_buffer_depth_ms (histogram, labels: direction)
siphon_ai_bridge_added_latency_ms (histogram, labels: stage)
  # stage ∈ {tap_to_ws, ws_to_inject}
```

**HEP:**
```
siphon_ai_hep_packets_sent_total{type}  # type ∈ {sip, rtcp, qos, log, cdr}
siphon_ai_hep_packets_dropped_total{reason}  # reason ∈ {collector_down, queue_full, encode_error}
siphon_ai_hep_collector_up
siphon_ai_hep_send_duration_seconds (histogram)
```

**System:**
```
siphon_ai_config_reloads_total{outcome}
siphon_ai_routes_count
siphon_ai_build_info{version, commit, rust_version}  # gauge=1
```

**Cardinality discipline:** never label by `call_id` directly. For per-call detail, use traces or HEP. The `call_id_hash` label uses `xxhash(call_id) % 1000` to give bounded cardinality for spotting outliers without exploding the metric store.

### 11.3 Distributed Tracing

OpenTelemetry, OTLP gRPC export to `observability.trace_endpoint`. Sampling controlled by `observability.trace_sample_ratio`.

**Span hierarchy per call:**

```
call (root span; lifetime = entire call)
├── attributes: call_id, route, register_source, sip_call_id, from, to, direction, codec
├── sip.invite_received       (span)
│   └── sip.acl_check
├── routes.match               (span; attribute: matched_route)
├── sip.sdp_negotiate          (span)
├── media.session_start        (span)
│   └── attributes: rtp_local_port, codec_negotiated, sample_rate
├── bridge.ws_connect          (span)
├── bridge.start_sent          (event)
├── bridge.first_audio_in      (event; ts since call start)
├── bridge.first_audio_out     (event; ts since call start)
├── sip.invite_200_sent        (event)
├── ... (steady state — no spans during conversation; would balloon trace size)
├── barge_in.detected          (event, ×N)
├── dtmf                       (event, ×N)
├── transfer                   (span if transfer happens)
├── sip.bye_received_or_sent   (event)
├── media.session_end          (span; attributes: jitter_p50/p95, loss_ratio, packets_sent/recv)
└── bridge.ws_close            (span; attribute: close_reason)
```

The steady-state conversation does NOT generate per-frame spans. That would be 50 spans/sec/call. Instead, periodic span events every 30s carrying running stats.

**Trace context propagation:** the `start` WS message includes a `traceparent` field (W3C Trace Context). The developer's WS server can join the trace if they emit OTel themselves.

### 11.4 HEP/EEP to Homer

See §3.5 for architecture. Operational notes:

- **Capture ID** identifies SiphonAI as a HEP agent in Homer. Use a unique ID per node (or per cluster); document the convention in `docs/HEP.md`.
- **Correlation:** every emitted HEP packet carries chunk 0x0011 (correlation_id) = SiphonAI's internal `call_id`. This makes Homer's Call Flow view stitch SIP + RTCP + logs into one timeline.
- **Compression:** HEP3 supports payload compression (zlib). Off by default for low latency; enable for high-volume nodes via `hep.compression = "zlib"`.
- **Encryption:** payload encryption supported via `hep.password` (HEP3 chunk 0x000B). Required for any non-private-network deployment.
- **Failure mode:** if collector goes down, packets queue up to `hep.queue_max` (default 10,000) then drop. Drops are counted via `siphon_ai_hep_packets_dropped_total`. Never blocks the call.
- **Local development:** `examples/homer-stack/docker-compose.yaml` brings up Homer + Postgres + the HEPlify-Server stack. `docker compose up` and you can see your own calls in the Homer UI within seconds.

### 11.5 CDRs (Call Detail Records)

Generated at end-of-call. JSON Lines format by default (one CDR per line, append-only).

**CDR schema:**
```json
{
  "version": "1",
  "call_id": "siphon-7f3a9b21",
  "sip_call_id": "abc123@pbx.example.com",
  "node_id": "siphon-ai-01",
  "route": "main_reception",
  "register_source": "cucm-main",
  "direction": "inbound",
  "from": { "user": "+13125551234", "host": "carrier.example.com", "display": "John Doe" },
  "to":   { "user": "5000", "host": "siphon-ai.example.com" },
  "request_uri": "sip:5000@siphon-ai.example.com",
  "started_at": "2026-05-04T14:30:00.123Z",
  "answered_at": "2026-05-04T14:30:02.456Z",
  "ended_at": "2026-05-04T14:35:12.789Z",
  "duration_ms": 312333,
  "billable_seconds": 310,
  "answer_latency_ms": 2333,
  "outcome": "completed",
  "hangup_cause": "caller_hangup",
  "sip_hangup_code": null,
  "codec": "PCMU",
  "sample_rate": 8000,
  "bridge": {
    "ws_url": "wss://reception.example.com/sip-bridge",
    "ws_connect_ms": 89,
    "first_audio_in_ms": 145,
    "first_audio_out_ms": 1820,
    "audio_in_frames": 15500,
    "audio_out_frames": 14820,
    "underruns": 2,
    "barge_in_count": 3,
    "dtmf_in": ["1", "5"],
    "dtmf_out": []
  },
  "media": {
    "rtp_packets_sent": 15500,
    "rtp_packets_recv": 15498,
    "rtp_loss_ratio": 0.00013,
    "jitter_p50_ms": 8.2,
    "jitter_p95_ms": 22.1,
    "estimated_mos": 4.2
  },
  "transfer": null,
  "tags": []
}
```

**Sinks:** file (JSONL with rotation) and webhook (HTTP POST with retries). Both can be enabled simultaneously.

### 11.6 Lifecycle Webhooks

Out-of-band HTTP POSTs for ops integrations that need to know about events but aren't the WS bridge endpoint.

**Events:**
- `call_start` — fired immediately on INVITE accept (post-route-match, pre-200)
- `call_end` — fired at terminal state, payload is the CDR
- `registration_state_changed` — registration went up/down
- `ws_failure` — WS connection failed (couldn't connect, mid-call disconnect, server unreachable)
- `hep_collector_changed` — HEP collector reachability changed
- `config_reloaded` — successful or failed config reload

**Delivery:**
- POST with JSON body
- Bearer auth via `webhooks.auth_header`
- Retry with exponential backoff up to `webhooks.retry_max` times
- 5s timeout per attempt
- Failures logged but never block the daemon
- Per-event filter (only deliver subscribed event types)

**Payload includes** event type, timestamp, node_id, and event-specific fields. `call_end` payload IS the full CDR — no need to pull separately.

### 11.7 Health & Admin Endpoints

Two HTTP listeners separate from metrics for clean ops:

**`/health` on `observability.health_listen`** (liveness)
- Returns 200 if process is up and not in shutdown
- Used by k8s liveness probes, load balancers
- No auth (intentional — must be cheap to hit)

**`/ready` on `observability.health_listen`** (readiness)
- Returns 200 only when:
  - Config loaded successfully
  - All `[[register]]` blocks with `register_on_startup = true` are registered
  - HEP collector is reachable (if HEP enabled)
  - SIP listener is bound and accepting
- Returns 503 with JSON body listing failing checks
- Used by k8s readiness, deployment gates

**Admin API on `observability.admin_listen`** (default 127.0.0.1 only)
- `POST /admin/log-level` — temporarily change a log target's level: `{"target": "siphon_ai::bridge", "level": "debug", "duration_secs": 600}`
- `POST /admin/config/reload` — same as SIGHUP
- `GET /admin/calls` — list active calls (call_id, route, duration, state)
- `GET /admin/calls/:call_id` — detailed state for one call
- `POST /admin/calls/:call_id/hangup` — force-hangup a stuck call
- `GET /admin/registrations` — registration status for each `[[register]]`
- `POST /admin/hep/test` — send a synthetic HEP packet to verify collector reachability
- All admin endpoints require bearer token via `security.admin_token`

### 11.8 What "Detailed Logging" Means in Practice

For any call, you should be able to answer these questions from logs+traces+HEP alone, without attaching a debugger:

1. Why did this call hit this route? → log includes `route_matched` event with the matched fields
2. Why was this codec chosen? → log includes SDP offer/answer summary
3. Why did the call drop? → terminal log line includes `hangup_cause` and `sip_hangup_code`
4. Where was the latency? → trace shows time between each span/event
5. Was the WS server slow to respond? → `bridge.first_audio_out_ms` in CDR + WS message timing in trace
6. Did the caller experience audio quality issues? → RTP QoS metrics + HEP RTCP
7. What did the SIP exchange actually look like? → HEP→Homer Call Flow view
8. Did barge-in fire when expected? → events in trace, count in CDR
9. Did the WS server send an unexpected message? → debug log of every received WS message (under `siphon_ai::bridge=debug`)
10. Which call ended my registration? → registration state transitions logged with last call_id processed before failure

If any of these can't be answered from observability data, that's a v1 bug, not a post-launch enhancement.

---

## 12. Security (v1 minimum)

- **SIP:** digest auth supported (siphon-rs has it); IP ACLs in config
- **Rate limiting:** `max_calls_per_second`, `max_concurrent_calls`
- **WS:** TLS (`wss://`) supported; bearer token in `Authorization`
- **Secrets:** env-var expansion in YAML (`${VAR}`); never logged
- **No outbound calls in v1** = no toll fraud surface
- **TLS for SIP (SIPS):** siphon-rs supports it; expose in `sip.transport: [tls]`; document cert config

**Not in v1:**
- SRTP (forge has it; trivial to enable post-v1 once SDP negotiation is plumbed)
- Per-call WS auth tokens (single global token for v1)
- mTLS on WS

---

## 13. Risk Register

| ID | Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|---|
| R1 | forge-engine doesn't expose a clean tap; need significant upstream PR | Medium | High | Week-1 spike; treat as blocking; if it slips, add a week to plan |
| R2 | siphon-rs `sip-sdp` placeholder bites somewhere unexpected | Low | Medium | Use forge-sdp throughout; never call into siphon-rs SDP |
| R3 | Cisco CUCM has registration quirks (custom headers, specific OPTIONS handling) | High | Medium | Schedule CUCM testing in Week 4 with buffer; document workarounds |
| R4 | Audio drift over long calls | Medium | High | Monotonic playout clock from Week 1; long-call test in Week 5 |
| R5 | Barge-in feels laggy due to jitter buffer + VAD latency | Medium | Medium | Tune VAD aggressiveness; document tuning knobs; test perceptually |
| R6 | WS protocol design hits a v2-breaking change soon after release | Medium | Medium | Version field on `start`; document breaking-change policy; gather feedback early |
| R7 | Forge or siphon-rs change APIs in ways that break SiphonAI | High | Low | Pin git revs; coordinate releases since same author owns all three |
| R8 | NAT/SBC scenarios more complex than `sip.public_address` covers | Medium | Medium | Document v1 limitation; recommend deploying behind an SBC for carrier-facing use |
| R9 | Performance doesn't hit 500 cc on reference hardware | Low | Medium | Profile early (Week 5); kernel offload (forge-kernel) is escape hatch |
| R10 | HEP upstream PRs (siphon-rs, forge-media) take longer than expected | Medium | Medium | Start in Week 1; you own all three repos so review/merge friction is zero; fallback is to do the emission inside SiphonAI by shadowing parsed messages from siphon-rs (uglier but works) |
| R11 | Route matching gets gnarly with edge cases (header escaping, regex performance, registration source attribution) | Medium | Low | Comprehensive `routes` crate test suite from Week 2; document the matching grammar precisely |
| R12 | Homer collector unreachable in production silently degrades observability without anyone noticing | Medium | Medium | `siphon_ai_hep_collector_up` metric + alert; `/ready` returns 503 if HEP enabled and collector down for >30s |
| R13 | CDR webhook delivery to a slow endpoint creates back-pressure | Low | Medium | Bounded queue per webhook target; drop with metric increment; never block call teardown |

---

## 14. Definition of Done — v0.1.0

A reasonable user can:

1. `docker compose up` and have a SIP-to-echo bridge working in <5 min
2. Point an Asterisk trunk at SiphonAI and have calls bridged to their WS server
3. Register SiphonAI to a Cisco CUCM as a phone, dial that extension, and get bridged
4. Implement a WS server using only the protocol doc (no source-code spelunking required)
5. Run a 1-hour call without quality degradation
6. Use barge-in and have it feel responsive
7. Send and receive DTMF
8. Initiate a blind transfer from their WS server
9. **Route different incoming numbers/extensions/headers to different WS servers via TOML config**
10. **See their calls live in Homer with full SIP flow + RTCP correlated by call_id**
11. **Get a CDR for every call, written to disk and/or POSTed to a webhook**
12. **Receive lifecycle webhooks (call_start, call_end, registration changes) at an ops endpoint**
13. **Diagnose a problem call from logs+traces+HEP alone (the §11.8 ten questions)**
14. **Hit `/health` and `/ready` from k8s and get correct semantics**
15. **Bump a single log target to debug at runtime via the admin API without restarting**
16. Find an "OpenAI Realtime bridge" example WS server and have it work out of the box

---

## 15. Open Questions / Decisions to Revisit

These don't block kickoff but should be answered before the relevant week:

1. **Codec offer ordering:** v1 default is `[pcmu, pcma]`. Opus is *not* offered in v1 — its 48 kHz audio rate doesn't fit the bridge's PCM16 / 8k|16k contract, and adding resampling lives in forge-media, not here. Reopen when forge-resampler ships.
2. **WS reconnect mid-call:** Punted to post-v1. Worth gathering user feedback before designing.
3. **Outbound originated calls (UAC INVITE for `make this call`):** Not in v1, but WS server `transfer` is close — should there be a `dial` control message in v1.x?
4. **Recording:** Forge already has it. Easy add post-v1 — server requests recording via control message, file is stored locally or pushed to S3.
5. **License:** Apache-2.0 / MIT dual (matches siphon-rs and forge-media).
6. **HEP capture ID conventions:** single global vs. per-route vs. per-registration. Probably global per node, but Homer multi-tenant deployments may want per-route. Defer until first multi-tenant Homer user shows up.

---

## 16. What This Document Replaces

This supersedes the original outline's scope for Phases 1–8. Specifically:

- **Phase 1 SIP:** Largely delivered by siphon-rs sip-uas/sip-uac; SiphonAI just wires it
- **Phase 1 Media:** Fully delivered by forge-media; SiphonAI uses as a library
- **Phase 1 WS Bridge:** This is the actual work in SiphonAI — see §4 protocol spec
- **Phase 2 AI Bridge:** Removed entirely; that's the developer's WS server
- **Phase 3 Call Control:** Mostly delivered by siphon-rs; SiphonAI maps SIP events to WS protocol events
- **Phase 4 Media Maturity:** Inherited from forge-media
- **Phase 5 Scaling:** Forge already targets 1000+ sessions/node; SiphonAI inherits. Multi-node scaling is operationally trivial since each call is independent (round-robin or hash by Call-ID at L4)
- **Phase 6 Observability:** §11 above
- **Phase 7 Packaging:** Week 6
- **Phase 8 OSS Launch:** Week 6 + post-launch

The original outline's 12-week plan compresses to ~7 weeks because the two heaviest pieces (SIP stack, media engine) are already built. The added week vs. the previous 6-week estimate is for HEP/CDR/webhook/admin work — observability that was originally underscoped.
