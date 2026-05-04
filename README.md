# SiphonAI

A SIP-to-WebSocket media bridge written in Rust.

SiphonAI accepts inbound SIP calls (as a trunk endpoint or as a registered
phone on a PBX), streams the call's audio over a WebSocket to a developer-
supplied server, and plays audio received back over that WebSocket into the
call. **It does not contain any AI code** — that is the WebSocket server's job.

## Status

Pre-alpha; scaffold only. See `docs/DEV_PLAN.md` for the 7-week sprint plan.

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
| `test-harness/`       | SIPp scenarios, load tooling, HEP collector stub |
| `docs/`               | Protocol, config, dialplan, HEP, deployment |

## Building

```sh
cargo build --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

## Reading order for contributors

1. `CLAUDE.md` — operating instructions (read first; re-check before
   non-trivial changes).
2. `docs/DEV_PLAN.md` — what we're building and why.
3. `docs/PROTOCOL.md` — the WebSocket bridge protocol contract.

## Upstream dependencies

SiphonAI is glue. The heavy lifting lives in three companion repos owned by
the same author:

- [`siphon-rs`](https://github.com/thevoiceguy/siphon-rs) — RFC 3261 SIP stack
- [`forge-media`](https://github.com/thevoiceguy/forge-media) — RTP/codecs/SDP/jitter/VAD
- `hep-rs` — HEP3 codec, transport, `HepSink` trait (in progress; not yet a public repo)

## License

Dual-licensed under MIT or Apache-2.0, matching the upstream stack.
