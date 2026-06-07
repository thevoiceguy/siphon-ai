# Integrating SiphonAI with Twilio

Two ways Twilio can hand calls to siphon-ai:

1. **Elastic SIP Trunking** — Twilio routes inbound PSTN traffic
   straight to siphon-ai's public SIP address. The most natural
   shape; siphon-ai *is* the SIP endpoint. This page covers the full
   setup.
2. **Programmable Voice + `<Dial><Sip>`** — Your TwiML response
   dials a SIP URI that points at siphon-ai. Useful when calls hit
   Twilio numbers and TwiML decides whether to route to siphon-ai
   based on per-call logic. Covered briefly in the last section.

The runnable config is in [`examples/twilio-trunk/`](../examples/twilio-trunk/).

---

## Path 1 — Elastic SIP Trunking

### What you'll need

| Thing                      | Notes |
| -------------------------- | ----- |
| A Twilio account           | And an Elastic SIP Trunk created in the console. |
| A purchased Twilio number  | Or a port-in. Associated with the trunk. |
| A public IP or DNS name    | Reachable from Twilio's signalling IPs on UDP/5060 (or your chosen port). |
| Open firewall              | UDP on your SIP port (default 5060) for signalling, plus the RTP range from `[media].rtp_port_range` for media. Twilio's signalling and media IPs are different sets — open both. |
| siphon-ai 0.1.0 or later   | This recipe targets the v0.2.0 protocol. |

### 1. Configure the trunk's Origination URI on Twilio

In **Console → Elastic SIP Trunking → Trunks → (your trunk) →
Origination**, add an Origination URI that points at your
siphon-ai:

```
sip:siphon.example.com:5060;transport=udp
```

(Replace `siphon.example.com` with your public DNS / IP. Use `5061`
+ `transport=tls` for TLS once you've got that wired — see
"TLS" below.)

Origination is what Twilio uses for **inbound** PSTN → SIP traffic.
"Termination" is the other direction (SIP → PSTN) and isn't
relevant to siphon-ai as a v1 UAS.

### 2. Allowlist Twilio's signalling IPs

By default siphon-ai accepts INVITEs from anywhere on its bind
address — fine for local dev, dangerous in production. Pin inbound
traffic to Twilio's actual signalling IP ranges via a `[[trunk]]`
block:

```toml
[[trunk]]
name = "twilio"
sources = [
  # Twilio NA Virginia (example — check current list)
  "54.172.60.0/30",
  # Twilio Ireland
  "54.171.127.192/30",
  # …add the regions you accept calls from
]
```

The current authoritative list is published by Twilio at
[**SIP signaling IP addresses**](https://www.twilio.com/docs/sip-trunking#ip-addresses).
Refresh this list when Twilio adds a region or your account moves.

Calls from outside the allowlist get rejected with `403 Forbidden`
*before* any route matching or media setup runs (`crates/sip-glue`
trunk gate).

### 3. Point at your WebSocket server

```toml
[bridge]
ws_url = "ws://127.0.0.1:8765/"
ws_connect_timeout_ms = 3000

[[route]]
name = "twilio_inbound"
register_source = "twilio"
[route.match]
any = true
```

For a smoke test, run the
[`examples/echo-ws-server-python`](../examples/echo-ws-server-python/)
server on `127.0.0.1:8765` — the caller hears their own audio
played back. Replace with your real STT / LLM / TTS server when
you're ready.

### 4. The full minimal config

See [`examples/twilio-trunk/siphon-ai.toml`](../examples/twilio-trunk/siphon-ai.toml).
The placeholders are marked.

### 5. Verify the call path

With the daemon running and Twilio's trunk pointed at it, place a
test call. You should see:

- A `200 OK` round-trip in the daemon log:
  ```
  INFO siphon_ai_core::acceptor: accepted INVITE
  ```
- A `start` message land on the WebSocket server.
- Audio frames flow both ways.

If the call rings forever and never reaches your daemon:

- Check Twilio's call log for the SIP response code from your endpoint.
  `403` ⇒ trunk allowlist rejected the source IP. `404` ⇒ no route
  matched (add the `any = true` route in §3). `500` ⇒ siphon-ai
  internal — check the daemon log.
- `tcpdump -ni any -s 0 -A 'port 5060'` on the daemon host to
  confirm packets are actually arriving.

### TLS — recommended for production

The recipe above uses UDP for clarity. For production, change to
TLS:

```toml
[sip]
listen = "0.0.0.0:5060"
transports = ["tcp", "tls"]   # keep UDP/5060 listening too if you want

[sip.tls]
listen = "0.0.0.0:5061"
cert = "/etc/siphon-ai/tls/fullchain.pem"
key  = "/etc/siphon-ai/tls/privkey.pem"
```

And update Twilio's Origination URI to
`sip:siphon.example.com:5061;transport=tls`.

Twilio terminates TLS at the trunk — they validate your certificate
against the public CA chain. Use Let's Encrypt or your existing
internal PKI.

The deeper "cert provisioning, file permissions under the systemd
`siphon` user, Let's Encrypt deploy-hook for renewal, openssl + SIPp
smoke test" recipe is in [`docs/DEPLOY.md`](DEPLOY.md) § TLS
deployment (landed in 0.2.0). mTLS for the bridge WS leg and SRTP
for the media leg are still 0.2.1 / 0.3.0 — see CHANGELOG.

### Cross-checking STIR/SHAKEN against `X-Twilio-VerStat` (0.4.x)

US-originated calls reach your trunk already carrying Twilio's own
attestation verdict in the `X-Twilio-VerStat` SIP header — one of
`TN-Validation-Passed-A` / `-B` / `-C`, `TN-Validation-Failed-…`, or
`No-TN-Validation`. SiphonAI can **independently verify** the same call's
`Identity` header (fetch the `x5u` cert, validate the chain to the STI-PA
anchor, check the ES256 signature, the orig/dest TN binding, and `iat`
freshness) and surface the result as `start.verstat`. Comparing the two is
the cleanest sanity check during rollout: they should agree, and a wall of
disagreement usually means *your* config is off (e.g. a stale trust anchor),
not that Twilio is wrong.

Turn on verification and forward Twilio's header so your WS server sees both:

```toml
[bridge]
ws_url = "ws://127.0.0.1:8765/"
forward_headers = ["X-Twilio-VerStat"]   # surfaces on start.sip.headers

[security]
min_attestation = "none"                 # observe-only — don't gate yet

[security.stir_shaken]
enabled       = true
trust_anchors = "/etc/siphon-ai/sti-pa-roots.pem"   # the authentic STI-PA root(s)
```

Keep `min_attestation = "none"` until the verdicts agree in practice — that
way a divergence is logged, not call-rejecting. Then your WS server has, on
the `start` message:

- `start.verstat` — SiphonAI's independent verdict (trust the `attest` only
  when every boolean holds), and
- `start.sip.headers["X-Twilio-VerStat"]` — Twilio's claim.

The runnable [`examples/verstat-compare-py`](../examples/verstat-compare-py/)
server logs `AGREE` / `DIVERGE` per call so you can watch them line up
before you enable the `min_attestation` gate. Once you trust the setup,
raise `min_attestation` (and optionally `require_identity`) to start
rejecting — see [`docs/CONFIG.md`](CONFIG.md) `[security]`.

**What attestation does and doesn't mean** — `A` means the carrier
authenticated the caller's right to that number, *not* that the call is
trustworthy. See [`docs/SECURITY_STIR_SHAKEN.md`](SECURITY_STIR_SHAKEN.md).

---

## Path 2 — Programmable Voice + `<Dial><Sip>` (alternative)

If your call routing logic lives in TwiML rather than at the trunk
level, your TwiML response can dial a SIP URI at siphon-ai
directly:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<Response>
  <Dial>
    <Sip>sip:siphon.example.com:5060;transport=udp</Sip>
  </Dial>
</Response>
```

When the caller answers the Twilio number, Twilio fetches your
TwiML webhook, sees the `<Dial><Sip>`, and bridges the PSTN leg to
your siphon-ai endpoint. From siphon-ai's perspective the inbound
INVITE looks identical to the trunk case — same config, same
allowlist, same routes. Two differences worth knowing:

1. **Twilio is the originator of the INVITE**, but the
   originating IP is still Twilio's signalling pool (same allowlist
   works).
2. **You pay TwiML pricing on top of trunk pricing.** Elastic SIP
   Trunking is cheaper if your routing logic doesn't need TwiML.

When to pick this path:

- You already have TwiML in production and don't want to add a
  separate trunk.
- You need per-call routing decisions that depend on Twilio-side
  data (caller geo lookups, business-hours logic, etc.) without
  pushing them into siphon-ai's dialplan.

The siphon-ai config in [`examples/twilio-trunk/`](../examples/twilio-trunk/)
works unchanged for this path.

---

## Production checklist

Before pointing real traffic at siphon-ai:

- [ ] `[node].public_address` set to a real public IP / DNS name.
- [ ] `[sip].listen` bound to `0.0.0.0` (not `127.0.0.1`).
- [ ] Firewall: UDP/TCP on SIP port + UDP on the RTP range.
- [ ] `[[trunk]] sources` pinned to Twilio's current signalling IPs.
- [ ] WebSocket server running and reachable from siphon-ai.
- [ ] TLS configured (recommended).
- [ ] HEP / Homer running so you can see SIP traces correlated with
      siphon-ai's CDRs and webhooks — see [`docs/HEP.md`](HEP.md).
- [ ] systemd / runit / launchd unit watching the daemon — see
      [`docs/INSTALL_DEBIAN13.md`](INSTALL_DEBIAN13.md) for the
      Debian-13 install script.

If anything in this recipe has drifted from current Twilio reality,
PRs and issue reports welcome.
