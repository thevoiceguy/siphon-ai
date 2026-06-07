# SIPp signaling regression scenarios

End-to-end SIP scenarios driven by [SIPp](https://github.com/SIPp/sipp).
SIPp plays the role of the **caller (UAC)**; SiphonAI is the UAS under
test. The point of these scenarios is to validate signaling
correctness end-to-end: parse → route → media setup → SIP final →
in-dialog handling → teardown. Audio quality is out of scope here
(see `docs/DEV_PLAN.md` §10.3).

## Prerequisites

- `sipp` binary: `apt install sip-tester` (Debian/Ubuntu)
- SiphonAI built: `cargo build -p siphon-ai`
- A WS server on `ws://127.0.0.1:8765` (the daemon's `local-dev.toml`
  default). The echo example in `examples/echo-ws-server-python/`
  works for everything except the transfer scenario.

## Running

```bash
# All non-transfer scenarios:
./run-all.sh

# Include the blind-transfer scenario (needs a WS helper that emits
# BridgeIn::Transfer at the right moment — see blind_transfer.xml):
./run-all.sh --with-transfer
```

`run-all.sh` spawns a fresh daemon, runs every scenario serially,
captures the daemon log under `/tmp/`, and exits non-zero on any
failure. SIPp's per-scenario `*_errors.log` lives next to the XML on
failure.

You can also drive a single scenario by hand against an already-running
daemon:

```bash
sipp -sf basic_call_then_bye.xml -m 1 -p 5070 -s 1000 127.0.0.1:5060
```

## Scenarios

| File                                | What it validates                            |
|-------------------------------------|----------------------------------------------|
| `basic_call_then_bye.xml`           | Happy path: INVITE → 200 → ACK → BYE → 200   |
| `caller_cancels_during_setup.xml`   | RFC 3261 §9.2 — CANCEL races the 200 OK; 487/200 both acceptable |
| `unsupported_codec_488.xml`         | SDP with only G.722 → 488 Not Acceptable Here|
| `blind_transfer.xml`                | WS-initiated REFER, 202 + BYE teardown       |
| `stir_shaken_no_identity_428.xml`   | STIR/SHAKEN `require_identity`: INVITE with no `Identity` header → 428 Use Identity Header (stir_shaken phase) |
| `stir_shaken_attestation_403.xml`   | STIR/SHAKEN gate: `Identity` present but unverifiable (unreachable `x5u`) below `min_attestation = "A"` → 403 Forbidden (stir_shaken phase) |
| `stir_shaken_attestation_pass.xml`  | STIR/SHAKEN happy path: a fully-verifiable `Identity` (fresh PASSporT, real x5u fetch, chain to the test anchor) → **200 admitted** (stir_shaken phase). Templated — `__IDENTITY__` is substituted at run time; does not run standalone. |

The `stir_shaken_*` scenarios run in `run-all.sh`'s always-on
**stir_shaken** auxiliary phase. It builds + runs the
`gen_test_passport` example (a `siphon-ai-stir-shaken` example) to mint a
fresh CA + leaf + x5u TLS server cert + signed PASSporT, serves the leaf
over a local HTTPS `x5u` (stdlib `http.server` + `ssl`), and starts a
daemon with verification enabled (`require_identity = true`,
`min_attestation = "A"`) trusting the test CA as both the STI-PA anchor and
the `x5u_tls_extra_ca`. The 428/403 rejects happen before media (no x5u/WS
needed); the pass case is a full admitted call through the echo WS bridge.

`gen_test_passport` doubles as an operator lab tool:

```sh
cargo run -p siphon-ai-stir-shaken --example gen_test_passport -- \
    /tmp/rig "https://127.0.0.1:8443/leaf.crt" "+12155551212" "1000"
# writes ca.pem / leaf.crt / server.crt / server.key into /tmp/rig,
# prints the Identity header value to stdout
```

## Adding a new scenario

1. Copy the closest existing scenario and adjust the `<send>` /
   `<recv>` lines. SIPp's `[service]`, `[remote_ip]`, `[call_id]`,
   `[branch]`, `[peer_tag_param]` substitutions are documented in
   the SIPp manual.
2. Add the filename to the `scenarios=(...)` array in `run-all.sh`.
3. Run `./run-all.sh` locally to confirm it passes.
4. Update this table.

See also: `docs/DEV_PLAN.md` §10.2 (the v1 scenario list — not all of
those land in the same PR; see commit history for the running
backlog).
