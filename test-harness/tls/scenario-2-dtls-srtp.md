# Scenario 2 — DTLS-SRTP via SIP-WebRTC gateway

Validates that an inbound INVITE offering `UDP/TLS/RTP/SAVPF`
gets answered correctly, the DTLS handshake completes, SRTP
master keys derive, and audio flows in both directions.
**Fully manual** — the §11 risk in `DEV_PLAN_0.3.0.md` flagged
this as untestable via SIPp:

> "SIPp doesn't natively drive DTLS-SRTP. Either accept that the
> DTLS path is tested by hand against a real WebRTC bridge..."

We accepted hand-testing for 0.3.0. This doc captures the
procedure.

## Pick one of these peer setups

### Option A — Janus WebRTC gateway with SIP plug-in (recommended)

Heaviest setup, closest to production WebRTC interop.

```bash
# Docker quick-start
docker run --rm --name janus -p 8088:8088 -p 8089:8089 \
  -p 10000-10100:10000-10100/udp \
  canyan/janus-gateway
```

Configure Janus's `janus.plugin.sip.jcfg` to register against
SiphonAI's SIP listener. Then place a call via Janus's
SIP-gateway demo page (`http://<janus-host>:8088/demos/sip.html`),
which originates a WebRTC call from the browser → Janus
transcodes WebRTC ↔ SIP → INVITE lands on SiphonAI as
`UDP/TLS/RTP/SAVPF`.

### Option B — Asterisk with `chan_pjsip` WebRTC bridge

Lighter; if you already have Asterisk lying around.

```ini
; pjsip.conf — minimal WebRTC trunk back to SiphonAI
[siphon-ai]
type = endpoint
transport = transport-wss
dtls_auto_generate_cert = yes
media_encryption = dtls
dtls_setup = actpass
webrtc = yes
codecs = ulaw,alaw
context = from-siphon-ai
```

Then a SIP softphone or webphone connects to Asterisk over WSS,
Asterisk bridges to a SIP/UDP INVITE toward SiphonAI carrying
the `UDP/TLS/RTP/SAVPF` profile.

### Option C — Just two siphon-ai instances

Easiest setup, narrowest coverage (only validates that siphon-ai
talks to itself). Spin up a second daemon as a UAC originator
(via the registered-trunk path) with an outbound INVITE carrying
DTLS-SRTP. Note: outbound originated INVITEs are post-v1, so
this option doesn't actually exist yet — leaving the note here
so future-you doesn't reach for it.

## Procedure

1. **Configure SiphonAI** with:
   - `[media].srtp = "preferred"` (or `"required"`)
   - A `[[trunk]]` allowing the gateway's IP
   - A route catching the inbound

2. **Start the observers** as in scenario-1 (`journalctl -f`,
   `sngrep`, `tcpdump`).

3. **Place a WebRTC call** through the gateway. Click-to-call
   in Janus's demo page, or dial through Asterisk's webphone.

4. **Watch for the DTLS handshake** in the daemon log:

   ```
   INFO ... DTLS-SRTP offer accepted, fingerprint=...
   INFO ... DTLS handshake complete, SRTP context derived
   INFO ... bridge connected ws_url=...
   ```

5. **Confirm media flows** — speak through the WebRTC client,
   hear yourself (assuming you're pointed at an echo bot).

## What to assert in each observer

**Daemon log:**
- `DTLS-SRTP offer accepted` line with the remote
  `a=fingerprint:` hash
- `DTLS handshake complete` line within ~1 second
- No `dtls.*error` lines

**Sngrep (200 OK SDP):**
- `m=audio <port> UDP/TLS/RTP/SAVPF 0 8 101`
- `a=setup:passive` (we're the answerer)
- `a=fingerprint:sha-256 <our cert fingerprint>`
- `a=connection:new`

**Tcpdump (RTP port range):**
- First the DTLS handshake (ClientHello / ServerHello /
  Certificate / etc. — packet sizes 100-1500 bytes, varied)
- Then SRTP packets at ~172 bytes each (G.711 20 ms frames +
  SRTP auth tag)

## PASS criteria

- 200 OK has `UDP/TLS/RTP/SAVPF` + `setup:passive` +
  `fingerprint:sha-256 ...`
- DTLS handshake completes (log line within 1 s of ACK)
- Two-way audio audible

## Notes from DEV_PLAN

The §11 risk doc says:

> "DTLS-SRTP SIPp coverage. SIPp doesn't natively drive
> DTLS-SRTP. ... Decision: hand-test for 0.3.0, file an issue
> for the Rust harness."

The Rust-harness option (a small binary using `forge-rtp`'s
DTLS-SRTP loopback to drive an INVITE against a real siphon-ai
instance) would close this gap for CI. Not in 0.3.0; tracked
separately. Until then, this scenario is run by the human
gating the release.

## Common failure modes

| Symptom | Likely cause |
|---|---|
| 488 Not Acceptable Here | `srtp_mode = "off"` somewhere — check both global and route override |
| 200 OK has `setup:active` (not `passive`) | siphon-ai forced `setup:passive` per RFC 5763 §5 — if you see `active` something's wrong upstream in forge-sdp |
| `DTLS handshake complete` never logs | The `srtp.rs` verify-callback regression that forge-media#61 fixed. Make sure your `forge-media` rev includes that PR (check `Cargo.toml` rev) |
| Audio is one-way (you hear them, they don't hear you) | SRTP context derived but only for one direction. Check the DTLS handshake actually exchanged both endpoint's keys |
| Audio is silence-only | DTLS succeeded but SRTP context misapplied — drop to `srtp_mode = "off"` to see if plaintext audio works; if it does, the bug is in the SRTP unprotect path |
