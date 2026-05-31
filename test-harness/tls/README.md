# 0.3.0 TLS validation suite

The five scenarios in `DEV_PLAN_0.3.0.md` §10 that gate the
release. This directory drives them on a dev box and reports
clear PASS / FAIL / MANUAL CHECK NEEDED verdicts.

## What each scenario covers

| # | Scenario | Coverage | Automation |
|---|---|---|---|
| 1 | SDES against Twilio Elastic SIP Trunk | Outbound carrier SRTP via SDES (`a=crypto:`) | Semi — manual call placement, scripted validation |
| 2 | DTLS-SRTP via SIP-WebRTC gateway | Inbound `UDP/TLS/RTP/SAVPF` handshake → SRTP context derivation → audio | Manual — SIPp can't drive DTLS handshakes |
| 3 | mTLS WebSocket with SPKI-pinned client cert | `[bridge.tls]` connector, cert + pin verification | **Fully automated** — local PKI, local WSS server, local SIPp call |
| 4 | SIP/TLS cert rotation via `systemctl reload` mid-call | SIGHUP path, in-flight TLS calls survive, new connections see new cert | **Fully automated** — local PKI, two cert files, SIPp UAC with sustained call |
| 5 | REGISTER over `sip:host;transport=tls` to a TLS PBX | `[[register]] transport = "tls"` outbound | Semi — needs a TLS PBX (Asterisk/FreeSWITCH); scripted validation |

## Prerequisites

```bash
# Debian 13 base
sudo apt install -y openssl sip-tester python3-websockets jq

# SiphonAI built (release or debug fine; debug is faster to rebuild)
cargo build -p siphon-ai
```

For the fully-automated scenarios (3 + 4) that's everything. For
semi-automated and manual scenarios, see the individual scenario
files for additional prerequisites (Twilio account, TLS PBX, etc.).

## How to run

```bash
# Just the automatable subset (recommended first run)
./run-all.sh --auto-only

# All scenarios, with prompts for manual steps in 1/2/5
./run-all.sh

# Single scenario
./scenario-3-mtls-wss.sh
./scenario-4-cert-reload.sh
```

Each script:
- Pre-flights its requirements and bails clearly if anything's missing
- Spins up its own siphon-ai instance on an ephemeral port (so it
  doesn't fight your production service)
- Generates throwaway PKI under `./certs/<scenario>/` (gitignored)
- Tears everything down on exit, including `trap` on SIGINT

## How to interpret results

Each scenario prints one of three verdicts at the end:

```
  ✓ PASS — <one-line summary>
  ! MANUAL CHECK — <what the operator needs to confirm by eye>
  ✗ FAIL — <what was expected, what was observed>
```

For PASS verdicts the script also prints the load-bearing
observation (e.g. the parsed SDP answer line, the cert
fingerprint, the metric counter). For FAIL it prints enough
context to triage without re-running.

## Layout

```
test-harness/tls/
├── README.md                    — this file
├── run-all.sh                   — driver
├── lib/
│   ├── common.sh                — colour helpers, traps, ephemeral-port picker
│   └── gen-pki.sh               — generates CA + server + client certs
├── configs/
│   └── *.toml.template          — siphon-ai configs per scenario (ephemeral ports)
├── scenarios/
│   └── *.xml                    — SIPp scenarios per test
├── scenario-1-sdes-twilio.sh
├── scenario-2-dtls-srtp.md
├── scenario-3-mtls-wss.sh
├── scenario-4-cert-reload.sh
└── scenario-5-register-tls.sh
```

## Relationship to the SIPp regression suite

The regression suite in `test-harness/sipp-scenarios/` runs on
every PR and gates UDP / plaintext-RTP correctness. This TLS
suite is a **release gate**, not a per-commit gate — the
external dependencies (Twilio, TLS PBX, real WebRTC gateway)
make it impractical to wire into CI today. The fully-automated
subset (scenarios 3 + 4) is a candidate to fold into CI once
the test-PKI generator stabilises; tracked separately.
