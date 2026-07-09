# Design: protocol SDKs & machine-readable schemas (P1 theme)

> **Status: DECISIONS LOCKED (2026-07-09) — §6.** Same design-first cadence
> as recording compliance (→ v0.24–0.26) and observability (→ v0.21–0.23):
> design note → locked decisions → chunked PRs → tag-after-merge. The build
> follows §7; deviations get noted back here.

Theme: **P1 "Protocol SDKs & machine-readable schemas" from
`docs/ROADMAP.md`**, the next open theme now that recording compliance is
complete (consent + outbound → v0.26.0). The roadmap frames it as: *"the WS
protocol is the product contract, but every integrator hand-rolls JSON +
20 ms audio framing from prose."* Three sub-items:

1. **JSON Schema** for every WS message, generated from / checked against
   the Rust types in `crates/bridge`.
2. **Server SDKs** — TypeScript and Python — handling framing, audio, and
   the message envelope, so a bot author writes handlers, not wire code.
3. **Conformance suite + protocol testkit** — replay a scripted call
   against a candidate WS server and validate its behavior; doubles as a
   mock daemon for SDK CI.

The headline finding from the code survey: **the protocol surface is
unusually schema-friendly, and the corpus already exists.** Both wire enums
are internally tagged (`type`, snake_case) with **21 `BridgeOut` + 17
`BridgeIn` variants**; there are *no* manual `Serialize` impls, no
`serde(with=…)`, no untagged unions anywhere on the surface
(`crates/bridge/src/protocol.rs`, plus `VerificationResult` in
`crates/security`). The binary half is trivial to specify: raw PCM16-LE
frames, no header bytes. And `docs/PROTOCOL.md`'s 26 fenced JSON examples
are already kept byte-in-sync with 55 round-trip tests by policy
(`protocol.rs:680-684`) — a ready-made conformance corpus.

---

## 1. The gaps today

- **Every integrator re-implements the same 60 lines of wire code.** All
  five in-repo reference servers hand-roll the `isinstance(message, bytes)`
  / `msg["type"]` dispatch and the 20 ms re-framing:
  `echo-ws-server-python` (614 LOC), `openai-realtime-bridge-py` (486),
  `deepgram-llm-bot-node` (1004 — with ~95 lines of manual `Buffer`
  carry/re-framing), `transcription-server-py`, `openai-pipeline-bot-py`.
  Third parties start from the same prose.
- **The contract isn't machine-checkable.** `PROTOCOL.md` is the spec;
  nothing validates a server's messages against it, and nothing stops a
  server from sending `{"type": "hangup"}` with a typo'd field until a
  live call fails.
- **No schema artifact to point tools at.** No JSON Schema → no editor
  autocompletion, no `jsonschema`-based validation in integrators' CI, no
  codegen for languages we don't ship SDKs for.
- **Node is invisible to CI.** The Python echo server runs under the SIPp
  job, but no workflow installs Node — the 1004-line deepgram bot is
  entirely untested. A conformance harness fixes this structurally.

---

## 2. Sub-item 1 — JSON Schema (→ v0.27.0)

**Source of truth stays the Rust types; the schema is generated, committed,
and drift-checked** — the exact pattern of
`examples/observability` + `scripts/check-observability-metrics.py`.

- **`schemars` derive** on the `protocol.rs` types (and
  `VerificationResult`), behind a new `json-schema` cargo **feature** on
  `siphon-ai-bridge`/`siphon-ai-security` so the daemon binary carries
  zero new code. `schemars` is the theme's one new Rust dep, dev-path
  only. The survey found nothing that won't derive cleanly; watch-items:
  `char` DTMF digits get an explicit `maxLength: 1`, and the
  `skip_serializing_if` fields are already `default`-marked so they render
  optional.
- **Generator** = `cargo run -p siphon-ai-bridge --example gen-schema
  --features json-schema`, writing **`schemas/siphon-ai.v1.json`**: one
  bundle with `bridge_out` / `bridge_in` `oneOf` unions discriminated on
  `type`, shared `$defs`, and `x-` annotations for the non-JSON half
  (binary frame sizing: PCM16-LE, 320 B @ 8 kHz / 640 B @ 16 kHz, 20 ms).
- **CI drift guard** `scripts/check-protocol-schema.py` (new job step in
  the `observability-artifacts`-style mold):
  1. regenerate and diff against the committed schema (schema ↔ Rust
     drift);
  2. **validate all 26 `PROTOCOL.md` JSON examples against the schema**
     (schema ↔ docs drift) — this closes the CLAUDE.md §4.2 loop with a
     third leg: Rust types ↔ round-trip tests ↔ PROTOCOL.md ↔ schema.
- Docs: `PROTOCOL.md` gains a §"Machine-readable schema" pointing at the
  artifact; `CLAUDE.md` §7.1/7.2 gain "regenerate the schema" as a step.

## 3. Sub-item 2 — Server SDKs, TypeScript + Python (→ v0.28.0)

Two small, dependency-light packages under **`sdks/`** (`sdks/python/`,
`sdks/typescript/`), each giving a bot author this shape:

```python
from siphon_ai_server import SiphonServer, Call

server = SiphonServer()

@server.on_call
async def handle(call: Call):
    async for event in call:            # typed BridgeOut events
        if event.type == "dtmf":
            await call.send_audio(pcm)  # 20 ms framing handled
    # call.hangup(), call.start_recording(), call.consent("dtmf-1"), …
```

- **Scope**: WS accept + `siphon-ai.v1` subprotocol echo, text/binary
  dispatch, typed message classes for all 38 variants, outbound audio
  **paced re-framer** (arbitrary byte pushes → exact 20 ms frames, the
  code every example hand-rolls today), `start` metadata surfaced,
  clean-close semantics (`hangup` vs bare close per PROTOCOL §5.7),
  reconnect-session awareness (`start.reconnected`). **Zero AI
  dependencies** — CLAUDE.md §4.1 applies to SDKs too.
- **Types are hand-written, schema-validated** (not codegen): 38 flat
  messages is a manageable surface, hand-written types stay idiomatic
  (TypeScript discriminated unions; Python dataclasses), and each SDK's
  test suite validates against **the schema + the PROTOCOL.md corpus** —
  parse every canonical example, re-serialize, `jsonschema`/`ajv`
  validate. Codegen toolchains (quicktype etc.) would be a third moving
  part for marginal gain at this size.
- **Dogfood: `echo-ws-server-python` is rewritten on the Python SDK.**
  It's the SIPp CI fixture, so every daemon PR then exercises the SDK
  end-to-end against real calls — the strongest possible regression net.
  A new minimal `examples/echo-ws-server-node/` lands on the TS SDK (the
  survey found the roadmap's assumed node echo server doesn't actually
  exist). The other examples migrate opportunistically, not in this
  release.
- **Packaging: vendorable, not yet published.** Proper `pyproject.toml` /
  `package.json` so `pip install ./sdks/python` and `npm install
  ./sdks/typescript` work (and publishing later is a metadata-only step),
  but no PyPI/npm release pipeline this theme — publishing adds account/
  signing/version-sync surface better decided after adoption feedback.

## 4. Sub-item 3 — Conformance suite + testkit (→ v0.29.0)

A new **`crates/protocol-testkit`** (bin: `siphon-ai-testkit`) that plays
the *daemon's* side of the protocol against a candidate WS server:

- **Scenario-driven**: TOML scenario files (bundled: `basic-echo`,
  `dtmf`, `recording-controls`, `hangup-semantics`, `keepalive`) — each a
  scripted call: connect, send `start`, stream N audio frames, inject
  events, and **assert** on the server's behavior: every JSON message
  validates against `schemas/siphon-ai.v1.json`, unknown-message
  tolerance, binary frames correctly sized/paced, close semantics.
- **Reuses the real types**: depends on `siphon-ai-bridge` for
  `BridgeIn`/`BridgeOut` (types only — its own thin tungstenite client,
  not the call-machinery-entangled bridge conn).
- **Doubles as the mock daemon for SDK CI**: `siphon-ai-testkit run
  --scenario all ws://localhost:8765` is exactly what the SDKs' CI runs.
  New CI job: build testkit, `setup-node` + Python, boot both SDK echo
  servers, run the full scenario set against each — **this is what
  finally puts Node in CI.**
- Exit code + JUnit-ish JSON report so third parties can gate their own
  CI on it: *"conformant with protocol v1"* becomes a testable claim.

## 5. What this theme is NOT

- **No protocol changes.** Protocol stays v1 throughout; the schema
  *describes* the existing wire, it never drives changes to it.
- **No AI code in SDKs or examples** (§4.1) — SDKs stop at typed events
  and audio plumbing.
- **No client SDKs** (dialing side) and no languages beyond TS + Python —
  the schema is the extension point for Go/Java/etc.
- **No package publishing** (PyPI/npm) — vendorable only, revisit later.
- **No browser/WebRTC support** — server-side SDKs only.

## 6. Decisions (LOCKED 2026-07-09 — all six as recommended)

- **D1 — Schema pipeline**: `schemars` derive behind a `json-schema`
  feature (dev-path only; the one new Rust dep), generated
  `schemas/siphon-ai.v1.json` committed, drift-checked in CI, and
  PROTOCOL.md's 26 examples validated against it. **Locked: yes.**
- **D2 — SDK shape**: hand-written idiomatic types (TS discriminated
  unions / Python dataclasses) validated against the schema + corpus in
  each SDK's tests; no codegen toolchain. **Locked: yes.**
- **D3 — Dogfooding**: rewrite `echo-ws-server-python` on the Python SDK
  (the SIPp job then exercises the SDK on every PR); add a new
  SDK-based `echo-ws-server-node`. **Locked: yes.**
- **D4 — Packaging**: vendorable packages, publishing deferred.
  **Locked: yes.**
- **D5 — Testkit**: Rust `crates/protocol-testkit` bin, TOML scenarios,
  schema-validating assertions, doubles as the SDK-CI mock daemon; new CI
  job wires Node in. **Locked: yes.**
- **D6 — Release slicing**: v0.27.0 schema + drift CI → v0.28.0 SDKs +
  example rewrites → v0.29.0 conformance testkit + CI job.
  **Locked: yes.**

## 7. Build order

1. **v0.27.0** — schemars derives + generator example + committed schema +
   `check-protocol-schema.py` (drift + corpus validation) + docs. Verify:
   CI catches a deliberately-drifted schema; all 26 examples validate.
2. **v0.28.0** — `sdks/python` + `sdks/typescript` + echo-python rewrite +
   new node echo example. Verify: SIPp suite green on the SDK-based echo
   server (the real gate); SDK unit suites validate the corpus; manual
   smoke per CLAUDE.md §5.3.
3. **v0.29.0** — testkit crate + bundled scenarios + conformance CI job
   running both SDK echo servers. Verify: testkit passes against both
   SDKs; deliberately-broken server fails with a readable report; theme
   retrospective back into this note.
