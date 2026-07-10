# CLAUDE.md

Operating instructions for AI coding agents (Claude Code, Cursor, etc.) working in this repo.
**Read this file first, before any task. Re-check relevant sections before non-trivial changes.**

For the *what* and *why* of this project, see `docs/DEV_PLAN.md`. This file is the *how*.

---

## 1. What SiphonAI Is (One Paragraph)

SiphonAI is a SIP-to-WebSocket media bridge written in Rust. It accepts inbound SIP calls (either as a trunk endpoint or as a registered phone on a PBX), streams the call's audio over a WebSocket to a developer-supplied server, and plays audio received back over that WebSocket into the call. **It does not contain any AI code.** AI is the WebSocket server's job. SiphonAI's job is the bridge: SIP signaling, RTP media, codec handling, jitter, barge-in, DTMF, hold/transfer — all wrapped in a clean WS protocol.

**If you find yourself writing code that calls an AI provider (OpenAI, Anthropic, Deepgram, ElevenLabs, etc.), stop. That's the wrong layer. Ask the user.**

---

## 2. Architecture in 60 Seconds

```
PBX/Trunk ──SIP/RTP──► SiphonAI ──WebSocket──► Developer's WS server (BYO AI)
                          │
                          ├── siphon-rs        (SIP stack — external dep)
                          ├── forge-media      (RTP, codecs, SDP — external dep)
                          └── our code         (orchestration + WS protocol)
```

Two upstream Rust libraries do most of the heavy lifting:

- **siphon-rs** (https://github.com/thevoiceguy/siphon-rs) — RFC 3261 SIP stack. UAS, UAC, transactions, dialogs, REGISTER, REFER, auth.
- **forge-media** (https://github.com/thevoiceguy/forge-media) — RTP/RTCP, codecs (G.711/Opus), SDP (`forge-sdp`), jitter buffer, DTMF, audio injection, VAD.
- **hep-rs** (new, owned by us) — HEP3 codec, transport, `HepSink` trait. Used by all three of siphon-rs (SIP messages), forge-media (RTCP/QoS), and siphon-ai (logs/CDRs/events) to ship observability data to Homer.

**All three are owned by the same author as SiphonAI.** If you find a missing capability in any, the right answer is often a small PR upstream, not a workaround here. Ask the user before doing this.

**SiphonAI itself is the thin orchestration layer + the WebSocket protocol.** Most code in this repo is glue, state machines, config, and the WS protocol implementation.

For the full design rationale, see `docs/DEV_PLAN.md`.

---

## 3. Workspace Layout

```
siphon-ai/
├── Cargo.toml                    # workspace root
├── crates/
│   ├── core/                     # CallController, state machine, glue
│   ├── bridge/                   # WS client + protocol types + audio bridging
│   ├── sip-glue/                 # Adapter: siphon-rs events → core
│   ├── media-glue/               # Adapter: forge-engine → core (the "tap")
│   ├── routes/                   # Route matching engine (TOML dialplan → bridge config)
│   ├── cdr/                      # CDR generation (JSON), file sink, webhook sink
│   ├── webhooks/                 # Out-of-band lifecycle webhooks (HTTP POST)
│   ├── config/                   # TOML config + validation + reload
│   └── telemetry/                # tracing + metrics + HEP wiring + admin/health endpoints
├── bins/
│   └── siphon-ai/                # The daemon binary
├── sdks/
│   ├── python/                   # Server SDK (siphon-ai-server on PyPI-style layout)
│   └── typescript/               # Server SDK (siphon-ai-server npm-style layout)
├── examples/
│   ├── echo-ws-server-python/    # Reference WS server (echo) — built on sdks/python
│   ├── echo-ws-server-node/      # Same in Node — built on sdks/typescript
│   ├── openai-realtime-bridge-py/  # Reference: OpenAI Realtime bridge
│   └── homer-stack/              # Local Homer + dashboards via docker-compose
├── docker/
├── docs/
│   ├── DEV_PLAN.md               # The plan — read for context
│   ├── PROTOCOL.md               # WS protocol v1 spec (canonical)
│   ├── CONFIG.md                 # TOML config reference (every field documented)
│   ├── DIALPLAN.md               # Route matching semantics with examples
│   ├── HEP.md                    # HEP integration setup and Homer correlation
│   ├── DEPLOY.md
│   ├── REGISTRATION.md
│   └── design/                   # design notes + historical dev plans (internal)
└── test-harness/
    ├── sipp-scenarios/           # SIPp test scripts
    ├── hep-collector-stub/       # Tiny HEP3 receiver for tests
    └── interop/                  # Lab notes for Asterisk/CUCM
```

### Where Things Go

| If you're adding... | Put it in... |
|---|---|
| WS protocol message types or serialization | `crates/bridge/src/protocol.rs` |
| WS connection lifecycle, reconnect logic | `crates/bridge/src/conn.rs` |
| Mapping SIP dialog events to call state | `crates/sip-glue/` |
| Mapping forge audio frames to/from WS | `crates/media-glue/` |
| The call state machine itself | `crates/core/src/call.rs` |
| Route matching logic, match grammar | `crates/routes/` + update `docs/DIALPLAN.md` |
| New config field | `crates/config/src/` + update `docs/CONFIG.md` + example TOML in `docs/` |
| New CDR field | `crates/cdr/src/schema.rs` + bump CDR `version` + update `docs/` |
| New webhook event type | `crates/webhooks/src/events.rs` + update `docs/` |
| Server SDK change (typed events, `Call` commands, framing) | `sdks/python` + `sdks/typescript` — keep both in lockstep; their tests validate against `schemas/siphon-ai.v1.json` + `docs/PROTOCOL.md` |
| New HEP chunk emission | `crates/telemetry/src/hep.rs` (uses `hep-rs` crate) |
| New metric | `crates/telemetry/src/metrics.rs` + document in `docs/DEPLOY.md` |
| New admin endpoint | `crates/telemetry/src/admin.rs` + document in `docs/DEPLOY.md` |
| New CLI flag | `bins/siphon-ai/src/main.rs` |
| Integration test against a real SIP scenario | `test-harness/sipp-scenarios/` |
| Test that needs a HEP collector | `test-harness/hep-collector-stub/` |

**If you're not sure where something goes, ask. Don't guess and create a new module.**

---

## 4. The Cardinal Rules

### 4.1 Never Add AI Code Here

SiphonAI is provider-neutral. Zero dependencies on AI vendors. If a feature seems to require AI integration, the answer is almost always "expose a hook in the WS protocol so the server can do it." Examples:

- Want speech detection? Already have VAD via forge — just emit `speech_started` events.
- Want transcription? **No.** The WS server runs STT.
- Want TTS? **No.** The WS server runs TTS and sends audio back over the WS.
- Want function calling? **No.** That's a server concern.

The `forge-ai-stream` crate exists in forge-media. **Do not depend on it from SiphonAI.**

### 4.2 The WebSocket Protocol Is a Public API

The protocol in `docs/PROTOCOL.md` is the contract third-party developers build against. Treat it like a published API:

- **Don't change message shapes without bumping the protocol version** (`version` field on the `start` message)
- **Don't add fields silently** — every field gets documented in `PROTOCOL.md` in the same PR
- **Don't introduce breaking changes without explicit user approval**
- **`seq` numbers are monotonic** on SiphonAI→server messages; never reset within a call
- **Audio frames are exactly 20ms** (160 samples @ 8kHz, 320 @ 16kHz) — not 10ms, not 30ms, not "approximately 20ms"
- **PCM16 little-endian, mono** — always. No surprise stereo, no big-endian, no float samples.

If the protocol needs to change, propose the change in a comment and wait for user input before implementing.

### 4.3 The Audio Hot Path Is Sacred

Per-call audio runs at 50 frames/sec (one every 20ms). On the audio path:

- **No allocations in the steady-state frame loop.** Reuse buffers. If you must allocate, pool it.
- **No `.unwrap()` and no `panic!`** — a panic on an audio task kills the call. Return an error and let the call tear down cleanly.
- **No locks shared across the audio path.** Use `tokio::sync::mpsc` channels between tasks. Never `std::sync::Mutex` on anything an audio task touches.
- **No blocking I/O.** Logging via `tracing` is fine; file I/O, sync HTTP, anything sync — not fine.
- **No `tokio::time::sleep` in the playout loop.** Use `tokio::time::interval` driven by a monotonic clock — not a self-correcting sleep, which drifts.

If you're tempted to add a feature in the hot path, pause and ask whether it can be done off-path (in a control task, in a separate per-call task, or in the WS server).

### 4.4 Never Share Per-Call State Across Calls

Each call is one `CallController` task with its own owned state. There is no global mutable call state. There is no "calls registry" that lets one call peek at another. Multi-node scaling depends on this — break it and you break horizontal scaling.

The one exception: process-wide metrics, which are atomic counters. Anything else, ask first.

### 4.5 Observability Is Not Optional

If you add a feature, it gets observability **in the same PR** as the feature, not later. Specifically:

- **Logs:** every state transition or external interaction logs at `info` (lifecycle) or `debug` (mechanics) with `call_id` in the span
- **Metrics:** new code paths get counters/histograms with bounded-cardinality labels (no `call_id` as a label — use `call_id_hash` or rely on traces/HEP for per-call detail)
- **Traces:** new long-running operations get spans; new significant moments get span events
- **HEP:** if the feature involves SIP messages or RTCP, HEP emission is automatic via `HepSink` — but if the feature involves an event Homer should know about, add a chunk emission for it
- **CDR:** if the feature affects per-call outcome, add a field to the CDR schema (and bump version)

The §11.8 "ten questions" in `docs/DEV_PLAN.md` is the bar. If your feature could prevent any of those from being answerable, you're not done.

### 4.6 Configuration Is TOML, And Routes Are Ordered

The single config file is TOML. Don't add YAML/JSON loaders. Don't add multi-tenancy.

For the route system:

- **Order matters.** Routes evaluate top-down; first match wins. Don't introduce priority numbers, scoring, or weighted matching — order in the file IS the priority
- **A default route (`any = true`) is required at the end** of any production config — log a warning at startup if missing
- **All match keys within a route AND.** No OR within a route — use multiple routes for OR
- **`regex = true` is per-route**, not per-match-key — keep the matching grammar predictable
- **Per-route overrides only override.** Anything not specified inherits from globals — never have a route silently ignore a global setting
- **Validation is at config load time**, not first-use — invalid regex, missing register_source reference, unset env var → fail loud at startup

### 4.7 HEP Emission Is Best-Effort, Always

HEP is observability, not call control. Therefore:

- **HEP emission never blocks the call path.** Sink methods are non-blocking; full queue → drop with metric increment
- **Collector unreachable is not a fatal error** — bump `siphon_ai_hep_collector_up{}` to 0, keep going
- **Don't log every dropped HEP packet** — that's a metric, not a log line. One warning per minute max if drops are happening
- **Never emit synchronously from the audio path.** Forge's RTP recv loop calls `sink.send_rtcp()` which queues and returns — the actual send happens in the HEP worker task

### 4.8 Use Upstream Capabilities Before Reimplementing

Before writing SIP, RTP, codec, SDP, jitter, VAD, DTMF, or HEP encoding code in this repo, **check whether siphon-rs, forge-media, or hep-rs already does it.** They almost certainly do. The most common failure mode for an AI agent here is reimplementing something that already exists upstream.

If something is missing upstream, the options in order of preference are:
1. Open a small PR upstream (ask user first)
2. Add a thin adapter in `sip-glue`, `media-glue`, or `telemetry`
3. Implement it in SiphonAI directly (last resort, requires user approval)

---

## 5. Build, Test, Run

### 5.1 Common Commands

```bash
# Build everything
cargo build --workspace

# Build the daemon
cargo build -p siphon-ai --release

# Run all tests
cargo test --workspace

# Run tests for one crate
cargo test -p siphon-ai-bridge

# Lint (CI runs this; run before pushing)
cargo clippy --workspace --all-targets -- -D warnings

# Format (CI runs this; run before pushing)
cargo fmt --all

# Run the daemon locally with example config
cargo run -p siphon-ai -- --config configs/local-dev.toml

# Run with verbose tracing
RUST_LOG=siphon_ai=debug,siphon=info,forge=info cargo run -p siphon-ai -- --config configs/local-dev.toml

# Bring up the full local stack (siphond as fake PBX + SiphonAI + echo WS + Homer)
docker compose -f docker/compose.yaml up
```

### 5.2 Running Integration Tests

```bash
# Requires SIPp installed (apt install sip-tester)
cd test-harness/sipp-scenarios
./run-all.sh
```

### 5.3 Manual Smoke Test

After any non-trivial change, run this before declaring done:

```bash
# Terminal 1: start echo WS server
cd examples/echo-ws-server-python && python server.py

# Terminal 2: start SiphonAI
cargo run -p siphon-ai -- --config configs/local-dev.toml

# Terminal 3: place a call (or use a softphone like Linphone)
sipp -sn uac 127.0.0.1:5060 -m 1 -s 1000
```

You should hear silence echoed back (or speak into the softphone and hear yourself). If you don't, the change broke something.

---

## 6. Coding Conventions

### 6.1 Rust Style

- **Edition 2021.** MSRV is whatever siphon-rs and forge-media require — check their `rust-toolchain.toml` if present.
- **`thiserror` for library crates, `anyhow` for binaries.** Don't mix.
- **No `unsafe`** without a `// SAFETY:` comment explaining the invariant. Default answer is "don't use unsafe."
- **No `unwrap()` outside tests.** `expect("clear message")` is acceptable for genuine invariants; `?` for everything else.
- **Errors carry context.** Use `anyhow::Context` or custom error variants — never bare `Err(e)?`.
- **Public APIs get doc comments** with at least a one-sentence description. Examples in doc comments are encouraged for non-obvious APIs.
- **Modules:** prefer `mod foo; pub use foo::Foo;` re-exports over deeply nested paths in the public API.

### 6.2 Async Patterns

- **One task per concern, communicate via channels.** A `CallController` spawns sub-tasks for SIP events, media in, media out, WS recv, WS send.
- **Use `tokio::select!` for tasks that wait on multiple sources** (e.g., WS recv + shutdown signal).
- **Always handle task cancellation cleanly.** A call ending should drop the controller, which should `Drop`-impl its way to clean teardown — or use explicit shutdown channels with select.
- **Don't `tokio::spawn` and forget.** Either store the `JoinHandle` for cancellation, or wrap in a structure that cleans up on drop.
- **Channel sizing:** audio channels get bounded buffers sized for ~200ms of audio (10 frames). Control channels get small bounded buffers (typically 8). Never `unbounded_channel` on the audio path.

### 6.3 Logging

```rust
use tracing::{info, debug, warn, error, instrument};

// Every per-call function gets instrumented with call_id
#[instrument(skip(self), fields(call_id = %self.call_id))]
async fn handle_invite(&mut self, invite: Invite) -> Result<()> { ... }

// Logs include structured fields, not just formatted strings
info!(target: "siphon_ai::call", from = %invite.from, "received invite");

// NOT this:
// println!("got invite from {}", invite.from);
// info!("got invite from {} for call {}", invite.from, call_id);  // call_id should be a span field
```

**Log levels:**
- `error!` — call failed, system-level problem
- `warn!` — recoverable problem, degraded behavior
- `info!` — significant lifecycle events (call started, ended, registered, transferred)
- `debug!` — dialog-level events, state transitions
- `trace!` — per-frame audio (off by default; never enable in prod)

### 6.4 Tests

- **Unit tests live in `#[cfg(test)] mod tests` at the bottom of the file under test.**
- **Integration tests** for a crate go in `crates/<name>/tests/`.
- **Cross-crate integration tests** that need real SIP/RTP go in `test-harness/`.
- **Use `tokio::test`** for async tests.
- **Mocks:** prefer trait-based mocks defined in the same crate. Avoid `mockall` unless the trait surface is large.
- **Audio test data:** use generated tones or silence; don't commit binary audio files unless absolutely necessary.

### 6.5 Comments

- **Comment the *why*, not the *what*.** The code shows what; comments explain why a non-obvious choice was made.
- **`// TODO:` is allowed only with a tracking issue:** `// TODO(#123): handle re-INVITE during transfer`
- **`// HACK:` and `// FIXME:` similarly require an issue link**
- **Don't leave commented-out code.** Delete it; git remembers.

---

## 7. Doing Common Tasks

### 7.1 Adding a New WS Control Message (Server → SiphonAI)

1. Add the variant to `BridgeIn` in `crates/bridge/src/protocol.rs`
2. Add a handler match arm in the WS receive loop in `crates/core/src/call.rs`
3. Implement the handler — usually translates to a `CallController` method or a `MediaTap` call
4. Add unit test for serialization round-trip
5. Add integration test in `test-harness/` if it touches SIP or audio
6. **Document it in `docs/PROTOCOL.md`** (this is not optional)
7. **Regenerate the protocol schema**: `cargo run -p siphon-ai-bridge --example gen_schema --features json-schema > schemas/siphon-ai.v1.json` (CI diffs it and validates the PROTOCOL.md examples against it)
8. **Update the server SDKs** in `sdks/python` + `sdks/typescript` (typed message classes + `Call` method for BridgeIn commands; their test suites assert full coverage of the schema's unions, so CI fails if you skip this)
9. **Update example WS servers** in `examples/` to demonstrate or at least handle the new message
10. If the message is a breaking addition (changes existing behavior), bump protocol version

### 7.2 Adding a New WS Event (SiphonAI → Server)

Same pattern, but `BridgeOut`. Plus:
- Decide whether the event needs to be opt-in via config
- Make sure `seq` is incremented atomically per-call

### 7.3 Adding a New Config Field

1. Add to the appropriate struct in `crates/config/src/`
2. Add `serde` default if optional
3. Add validation in `Config::validate()`
4. Update the example YAML in `docs/DEPLOY.md`
5. If it affects runtime behavior, add a smoke test

### 7.4 Adding a Metric

1. Define it in `crates/telemetry/src/metrics.rs` with the `siphon_ai_` prefix
2. Use the `metrics` facade — don't go direct to Prometheus types
3. Document it in `docs/DEPLOY.md` under the metrics section
4. Histograms get sensible buckets defined explicitly (don't rely on defaults)

### 7.5 Bumping Upstream Dep (siphon-rs, forge-media, hep-rs)

1. Update the git rev in workspace `Cargo.toml`
2. Run full test suite — **including** SIPp integration tests **and** HEP-collector-stub tests
3. If the upstream API changed, fix the glue layer (`sip-glue`, `media-glue`, or `telemetry`), not consumers
4. Document the bump reason in the commit message
5. If it's a coordinated change with the upstream repo, link both PRs in the commit

### 7.6 Adding a Route Match Key

1. Add the field to the `RouteMatch` struct in `crates/routes/src/match.rs`
2. Implement evaluation in `RouteMatch::matches()`
3. Add unit tests covering: exact match, regex match (when `regex = true`), no match, interaction with other keys
4. **Document in `docs/DIALPLAN.md`** with examples
5. Add a sample route to `docs/CONFIG.md`

### 7.7 Adding a CDR Field

1. Add the field to the schema struct in `crates/cdr/src/schema.rs`
2. **Bump the CDR `version` field** if the addition could break parsers (typically anything beyond an additive optional field)
3. Populate it from `CallController` at the appropriate lifecycle moment
4. Update example CDR in `docs/DEPLOY.md`
5. Update consumer-side documentation (webhook receivers will need to know)

### 7.8 Adding a HEP Chunk Emission

1. Confirm the chunk type is defined in HEP3 spec or vendor-specific (use vendor ID 0x0000 generic where possible)
2. Add the encoding in `hep-rs` (PR upstream if missing)
3. Wire emission from the appropriate layer:
   - SIP message → siphon-rs (`sip-hep`)
   - RTCP/QoS → forge-media (`forge-hep`)
   - Application event/log/CDR → SiphonAI's `crates/telemetry/src/hep.rs`
4. Verify in Homer UI that the chunk appears correlated with the call
5. Document in `docs/HEP.md`

### 7.9 Adding a Lifecycle Webhook Event

1. Add variant to `WebhookEvent` enum in `crates/webhooks/src/events.rs`
2. Define the JSON payload struct
3. Fire it from the appropriate code site (e.g., `core::CallController` for call events)
4. Update the `events = [...]` whitelist in `docs/CONFIG.md`
5. Document the payload schema in `docs/DEPLOY.md`

---

## 8. Things That Are Out of Scope

If asked to do any of these, **stop and confirm with the user before proceeding.** They're explicit non-goals.

- AI provider integration of any kind
- Multi-tenancy (note: route-based dispatch in a single config is in scope; multi-tenant separation is not)
- Video
- WebRTC client support
- WS reconnect mid-call (post-v1). Note: park/retrieve (0.7.0) is **not** this — retrieve opens a *fresh* WS session on an operator action, it does not resume a dropped one.
- SRTP beyond what's shipped: DTLS-SRTP is produced (0.3.0); SDES and mid-call re-key are not.
- Custom codec implementations (use forge-codecs)
- Reimplementing SIP transactions, dialogs, or transports (use siphon-rs)
- Reimplementing HEP encoding (use hep-rs)
- YAML/JSON config formats (TOML only)
- Per-call log files (use structured logs + correlation by call_id)

**Delivered since the original v1 cut** (no longer out of scope — these shipped and have their own docs/config): call recording (0.5.0), outbound origination + attended transfer (0.6.0/0.6.1), conferencing/mixing and media-only call park (0.7.0). Don't refuse work on these as "out of scope"; treat them as supported features.

---

## 9. Doc & Plan Drift

This file and `docs/DEV_PLAN.md` should agree. If you make a decision that contradicts either:

1. Don't silently diverge — update the docs in the same PR
2. If the change is significant (e.g., a new dependency, a scope change), call it out in the PR description
3. If you're unsure whether a change crosses the threshold, ask

The protocol spec in `docs/PROTOCOL.md` is the highest-stakes document — treat changes there with extra care.

---

## 10. Quick Reference Card

| Question | Answer |
|---|---|
| Should I add AI code? | **No.** It's the WS server's job. |
| Can I change the WS protocol? | Only with a version bump and protocol doc update. Ask first. |
| Can I change the CDR schema? | Add fields freely; bump CDR `version` for any change that could break parsers. |
| Can I change the HEP chunk emissions? | Adding chunks is fine; removing/changing existing ones is breaking. Ask. |
| SIP/RTP problem — write it here? | Almost certainly belongs in siphon-rs or forge-media. Check first. |
| HEP encoding problem — write it here? | Belongs in `hep-rs`. Check first. |
| Can I `unwrap()`? | In tests, yes. In a binary's main, sometimes. Anywhere on the audio path, **no**. |
| Allocate in the audio loop? | No. Reuse buffers. |
| `std::sync::Mutex` on anything audio touches? | No. Use channels. |
| Block on the audio path for HEP/CDR/webhook? | No. Queue and let a worker handle it. |
| Add a metric labeled by `call_id`? | No. Use `call_id_hash` (xxhash mod 1000) or rely on traces/HEP for per-call detail. |
| Add YAML/JSON config support? | No. TOML only. |
| New dependency? | Ask first. We have a small dep tree on purpose. |
| Where do binary audio files go? | Nowhere — generate them in tests or use `examples/` fixtures. |
| `println!`? | No. Use `tracing`. |
| New feature without metrics/logs/traces? | No. Observability ships in the same PR as the feature. |
| Conventional Commits for messages? | Yes — `feat:`, `fix:`, `docs:`, `refactor:`, `test:`, `chore:`. |

---

## 11. When You're Stuck

If a task feels like it's in a gray area — scope creep, architectural shift, missing upstream capability, protocol change, performance trade-off — **stop and ask the user.** Document the question clearly:

> "Task X needs Y, which isn't covered by the current design. Options: (A) ..., (B) ..., (C) .... Recommendation: ___. Confirm before proceeding?"

A 30-second clarification beats a 2-hour rewrite.
