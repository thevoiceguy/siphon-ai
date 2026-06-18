# SIPp signaling regression scenarios

End-to-end SIP scenarios driven by [SIPp](https://github.com/SIPp/sipp).
SIPp plays the role of the **caller (UAC)**; SiphonAI is the UAS under
test (except `outbound_uas_answer.xml`, where the roles invert: SiphonAI
dials and SIPp answers). The point of these scenarios is to validate signaling
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
| `outbound_uas_answer.xml`           | 0.6.0 outbound answer path, roles inverted: SiphonAI dials via `POST /admin/v1/calls`, SIPp answers (180 → 200 + SDP), bridge runs, SiphonAI BYEs (outbound phase) |
| `outbound_srtp_uas_answer.xml`      | 0.7.1 outbound SRTP: gateway `srtp = "required"` makes SiphonAI offer `RTP/SAVP` + `a=crypto`; SIPp answers SAVP with its own `a=crypto`, keys install, call bridges (outbound_srtp phase) |
| `park_caller.xml`                   | 0.7.0 call park: caller is answered + bridged, the echo-ws parks it (`--auto-park`), and SiphonAI BYEs the caller when the park resolves. Shared by the **park_timeout** and **park_retrieve** phases |
| `conference_caller.xml`             | 0.7.0 conferencing: a caller that stays up while the echo-ws joins it to a room (`--auto-conference-join`); two run concurrently in the **conference** phase |
| `reinvite_hold_resume.xml`          | Peer-initiated hold: SIPp **sends** a sendonly re-INVITE then a sendrecv one; SiphonAI mirrors recvonly/sendrecv (RFC 3264 §6.1) |
| `bot_hold_caller.xml`               | 0.7.2 bot-initiated hold: the inverse — the echo-ws (`--auto-hold`) drives `hold`/`resume`, so SiphonAI **sends** the re-INVITEs and SIPp asserts it receives sendonly then sendrecv (**bot_hold** phase) |
| `outbound_bot_hold_uas.xml`         | 0.7.5 bot-initiated hold on an **outbound** leg: SIPp is the callee (UAS), the echo-ws (`--auto-hold`) drives `hold`/`resume`, and SIPp asserts it receives the sendonly/sendrecv re-INVITEs on the outbound Direct dialog (**outbound_bot_hold** phase) |
| `opus_caller.xml`                    | 0.8.0 Opus: SIPp offers `opus/48000/2` at a dynamic PT and asserts the 200 OK answers Opus **and** (0.8.2) carries our Opus fmtp re-keyed onto that PT (`a=fmtp:96 …stereo=0`); the daemon brings the call up as a 16 kHz bridge session (**opus** phase). Signalling only — SIPp can't encode Opus media. |
| `delayed_offer_caller.xml`           | 0.9.0 inbound delayed offer (offerless INVITE, the CUCM-without-MTP case): SIPp sends an INVITE with **no SDP** and asserts the 200 OK carries SiphonAI's own offer (`m=audio` + PCMU rtpmap, via `check_it`); SIPp then sends its SDP answer in the ACK and the call bridges + BYEs (**delayed_offer** phase). |
| `outbound_delayed_uas.xml`           | 0.9.0 **outbound** delayed offer, roles inverted: SiphonAI dials via `POST /admin/v1/calls` with `delayed_offer: true` (offerless INVITE); SIPp answers 200 with its own SDP **offer** and asserts (via `check_it`) the **ACK** carries SiphonAI's SDP **answer** (proving the gateway UAC's answer generator fired) (**outbound_delayed** phase). |
| `outbound_delayed_srtp_uas.xml`      | 0.9.1 outbound delayed offer **with SRTP on the answer**: gateway `srtp = "required"`; the offerless INVITE can't offer SRTP, so SIPp's 200 carries an `RTP/SAVP` + `a=crypto` SDES **offer** and SIPp asserts (via `check_it`) the **ACK** answers SRTP (`a=crypto`) — SiphonAI installed keys (**outbound_delayed_srtp** phase). |
| `delayed_offer_srtp_caller.xml`      | 0.9.2 inbound delayed offer **with SRTP on the offer**: `[media].srtp = "required"`; SIPp sends an offerless INVITE and asserts (via `check_it`) SiphonAI's 200 OK carries an SDES **offer** (`a=crypto`) — we're the offerer; SIPp answers SRTP in the ACK and the keyed call bridges (**delayed_offer_srtp** phase). |

`run-all.sh` also has an always-on **recording** auxiliary phase: it starts
a daemon with `[recording].mode = "always"` writing to a temp dir, runs one
`basic_call_then_bye.xml` call through the echo WS bridge, then asserts the
written file is a valid stereo PCM16 WAV with audio in it (via Python's
`wave`). It reuses `basic_call_then_bye.xml` — no dedicated scenario file.

`run-all.sh` also has an always-on **outbound** auxiliary phase — the
roles-inverted scenario above. It starts a fresh daemon with `[outbound]`
enabled and a `[[gateway]]` pointing at SIPp's port, a dedicated echo-ws
instance with `--auto-hangup-after-ms 1500` (so the WS side ends the call),
backgrounds SIPp as the callee, then POSTs `/admin/v1/calls`. Pass = SIPp
completed INVITE → ACK → BYE **and** the daemon's
`siphon_ai_outbound_calls_total{result="answered"}` metric reads 1.

And an always-on **outbound_srtp** auxiliary phase (0.7.1): the same setup
but the `[[gateway]]` sets `srtp = "required"`, so SiphonAI's INVITE offers
`RTP/SAVP` + `a=crypto` (SDES). `outbound_srtp_uas_answer.xml` answers SAVP
with its own `a=crypto`; SiphonAI installs keys and bridges. Pass = SIPp
completed the call **and** `siphon_ai_outbound_srtp_total{result="encrypted"}`
reads 1. (SIPp doesn't carry real SRTP media — it's a signalling/negotiation
test; the live key-install round-trip is covered by media-glue unit tests.)

`run-all.sh` has two always-on **park** auxiliary phases (0.7.0), both using
`park_caller.xml` and an echo-ws started with `--auto-park`:

- **park_timeout** — a daemon with `[park]` `timeout_secs = 1`,
  `timeout_action = "hangup"`. The call is parked, the timeout fires, and
  SiphonAI BYEs the caller. Pass = SIPp saw the BYE **and**
  `siphon_ai_parks_total{result="ok"}` reads 1.
- **park_retrieve** — `[park]` with no timeout. The runner backgrounds the
  caller, waits until it appears in `GET /admin/v1/parked`, then POSTs
  `/admin/v1/calls/:id/retrieve` onto a second echo-ws (`--auto-hangup-after-ms`).
  SiphonAI opens a fresh WS to it, that side hangs up, and SiphonAI BYEs the
  caller. Pass = SIPp saw the BYE **and**
  `siphon_ai_retrieves_total{result="ok"}` reads 1.

And an always-on **conference** phase (0.7.0): a daemon with `[conference]`
enabled and an echo-ws started with `--auto-conference-join`. Two
`conference_caller.xml` callers (on different ports) bridge to the same room.
Pass = the daemon mixes both (`siphon_ai_conference_participants` reads 4 —
two calls × SIP leg + WS session) **and** the room ends after both hang up
(`siphon_ai_conferences_active` returns to 0). SIPp can't assert mixed audio
content — that's covered by media-glue unit tests with synthetic PCM.

And an always-on **bot_hold** phase (0.7.2): a daemon and an echo-ws started
with `--auto-hold`, which drives a full bot-initiated hold cycle —
`hold` → ~1s → `resume` → `hangup`. SiphonAI is the re-INVITE *offerer*
(the inverse of `reinvite_hold_resume.xml`), so `bot_hold_caller.xml`
asserts it **receives** a sendonly re-INVITE then a sendrecv one and answers
each (recvonly / sendrecv). Pass = the SIPp scenario completed (both
`check_it` direction asserts held) **and** `siphon_ai_holds_total{result="ok"}`
reads 2 (one tick for hold, one for resume).

And an always-on **ws_reconnect** phase (0.7.3): a daemon with
`[bridge].ws_reconnect_enabled` and an echo-ws started with
`--drop-after-ms`, which abruptly closes the socket mid-call. SiphonAI keeps
the call up on hold music and re-dials the same `ws_url`; the redial's
`start` carries `reconnected: true`, the echo-ws hangs that resumed call up,
and SiphonAI BYEs the caller. Reuses `park_caller.xml` (answer + wait for the
server BYE). Pass = SIPp saw the BYE **and**
`siphon_ai_ws_reconnects_total{result="recovered"}` reads 1. (The exhaustion
path — no redial within the window — is covered by the controller unit test
`ws_reconnect_exhausts_and_tears_down`.)

And an always-on **outbound_reconnect** phase (0.7.4): the same drop +
reconnect, but on the **outbound** originate path — SiphonAI dials out
(`outbound_uas_answer.xml` as the callee), the echo-ws (`--drop-after-ms`)
drops, SiphonAI re-dials and resumes, and the call ends cleanly. Pass = SIPp
completed **and** `siphon_ai_ws_reconnects_total{result="recovered"}` reads 1.

And an always-on **outbound_bot_hold** phase (0.7.5): bot-initiated hold on
the **outbound** path — SiphonAI dials out (`outbound_bot_hold_uas.xml` as
the callee), the echo-ws (`--auto-hold`) drives `hold`/`resume`, and SiphonAI
sends the sendonly/sendrecv re-INVITEs on the outbound Direct dialog (via the
gateway UAC). Pass = SIPp completed (both direction asserts held) **and**
`siphon_ai_holds_total{result="ok"}` reads 2.

And an always-on **opus** phase (0.8.0): a daemon with
`[media].codecs = ["opus","pcmu"]`; `opus_caller.xml` offers Opus at a
dynamic PT and asserts (via `check_it`) the 200 OK answers Opus **and**
(0.8.2) carries our Opus fmtp re-keyed onto that PT (`a=fmtp:96 …stereo=0`).
Pass = SIPp completed **and** the daemon logged
`negotiated=opus sample_rate=16000` — Opus on the wire (`opus/48000/2`)
surfacing as a 16 kHz bridge session. Signalling only; the Opus encode/decode
round-trip is covered by forge-codecs / forge-engine unit tests.

And an always-on **delayed_offer** phase (0.9.0): a daemon with
`[media].codecs = ["pcmu"]` and the default `[sip].allow_delayed_offer =
true`. `delayed_offer_caller.xml` sends an INVITE with no SDP; SiphonAI
answers 200 OK carrying its own offer (asserted via `check_it`), SIPp
sends the answer in the ACK, the call bridges through the echo-ws and is
BYE'd. Pass = SIPp completed **and** the daemon logged the delayed-offer
accept. The error paths (ACK timeout, missing/invalid answer, no codec)
are unit-tested; SIPp covers the happy-path signalling contract.

And an always-on **outbound_delayed** phase (0.9.0 chunk 2): the
roles-inverted **outbound** direction. The runner POSTs
`/admin/v1/calls` with `delayed_offer: true`, so SiphonAI dials an
offerless INVITE through the `[[gateway]]`; SIPp
(`outbound_delayed_uas.xml`) answers 200 with its own SDP offer and
asserts (via `check_it`) that SiphonAI's **ACK** carries the SDP answer
its gateway UAC's answer generator built. Pass = SIPp completed **and**
`siphon_ai_outbound_calls_total{result="answered"}` reads 1.

And an always-on **outbound_delayed_srtp** phase (0.9.1): outbound
delayed offer where SRTP rides the **answer**. `[[gateway]].srtp =
"required"` makes encryption mandatory; since the offerless INVITE can't
carry an SDES offer, `outbound_delayed_srtp_uas.xml`'s 200 carries the
`RTP/SAVP` + `a=crypto` offer and asserts SiphonAI's **ACK** answers SRTP.
Pass = SIPp completed **and**
`siphon_ai_outbound_srtp_total{result="encrypted"}` reads 1. (DTLS-SRTP
on a delayed answer is not handled — SDES only.)

And an always-on **delayed_offer_srtp** phase (0.9.2): the inbound
mirror — SiphonAI **offers** SDES SRTP in the 200 OK because on an
inbound delayed offer *we* are the offerer. A daemon with `[media].srtp
= "required"`; `delayed_offer_srtp_caller.xml` sends an offerless INVITE
and asserts (via `check_it`) the 200 OK carries `a=crypto`, then answers
SRTP in the ACK so SiphonAI installs the key and the call bridges. Pass =
SIPp completed **and** the daemon logged the delayed accept. (DTLS-SRTP
on a delayed offer is not produced — SDES only.)

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
