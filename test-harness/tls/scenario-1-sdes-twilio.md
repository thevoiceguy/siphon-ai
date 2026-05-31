# Scenario 1 — SDES against Twilio Elastic SIP Trunk

Validates outbound carrier interop with SDES SRTP (`a=crypto:`
key exchange over the signaling plane). The §10-item-1 acceptance
line. **Hand-driven** — the call has to come from a real Twilio
number; the script can't synthesise that.

## Prerequisites

- Twilio account with an Elastic SIP Trunk **configured for TLS +
  Secure Media (SDES)** in the Twilio console:
  - Termination URI: `<your-host>;transport=tls`
  - Origination URI: same shape, pointing back at your SIP/TLS
    listener
  - Secure Media: ON
- The DID you'll call has its **Voice URL** pointing at the trunk
- SiphonAI deployed with:
  - `[sip].transports = ["tls"]` + `[sip.tls]` cert/key
  - `[media].srtp = "preferred"` (or `"required"` if you want to
    refuse plaintext-RTP fall-back)
  - A `[[trunk]]` block allowing Twilio's published edge IP ranges
    (`docs/TWILIO_INTEROP.md` has the current list)
- Tools on the host: `sngrep`, `journalctl`, `curl`

## Procedure

### 1. Verify the daemon is configured correctly

Run the supplied validation helper:

```bash
./scenario-1-sdes-twilio.sh --preflight
```

It greps your live `/etc/siphon-ai/siphon-ai.toml` and asserts:
- `[sip.tls]` is configured with readable cert/key files
- `[media].srtp` is `"preferred"` or `"required"`
- At least one `[[trunk]]` block covers a Twilio edge range

### 2. Start observers in three terminals

```bash
# T1 — daemon log filtered to SRTP / SDP / route lines
sudo journalctl -u siphon-ai -f \
  | grep -iE 'srtp|sdp|route matched|negotiated|crypto'

# T2 — live SIP capture (TLS body decrypted iff sngrep is built
# with libssl; otherwise this shows only the TLS handshake)
sudo sngrep -d any port 5061

# T3 — RTP packet capture; SRTP packets look like RTP at the
# header level but the payload is opaque ciphertext
sudo tcpdump -n -i any -c 50 'udp portrange 40000-40500'
```

### 3. Place a test call

Call your Twilio DID from any phone. Let it ring through to the
daemon and connect; speak for ~10 seconds; hang up.

### 4. Assert (what to look for in each terminal)

**T1 (daemon log):**
- `INVITE routed route=... register_source=twilio`
- A line mentioning `srtp_mode=preferred` (or `required`)
- A line mentioning `negotiated=...` with the SRTP profile
- `call ended ... cause=CallerHangup` after your hang-up

**T2 (sngrep):**
- The `200 OK` SiphonAI sends has an SDP body with
  - `m=audio <port> RTP/SAVP 0 8 101` (note `SAVP`, not `AVP`)
  - `a=crypto:1 AES_CM_128_HMAC_SHA1_80 inline:<base64-key>`

**T3 (tcpdump):**
- Bidirectional UDP packets between your RTP port range and a
  Twilio media IP (172.x or 168.86.x range)
- Packet sizes around 172 bytes (12-byte RTP header + 160-byte
  G.711 frame + ~10-byte SRTP auth tag)

### 5. Run the post-call validation helper

```bash
./scenario-1-sdes-twilio.sh --postcall
```

Pulls the most recent call from `/var/log/siphon-ai/cdr.jsonl`
and asserts:
- `direction = "inbound"`
- `route = "twilio-..."`
- `audio.codec = "PCMU"` (or PCMA)
- `termination.cause = "CallerHangup"` or `"local_shutdown"`

(Note: the CDR schema doesn't yet carry an explicit `srtp`
field — that's a 0.3.1 candidate. The signal for SDES today is
in the SDP answer body, observable via sngrep.)

## PASS criteria

- Daemon log shows `srtp_mode=preferred` and a negotiated
  SRTP profile
- Sngrep shows the 200 OK SDP using `RTP/SAVP` and including
  `a=crypto:` lines
- Two-way audio was actually audible
- CDR record completes with the expected route and termination

## Common failure modes

| Symptom | Likely cause |
|---|---|
| 488 Not Acceptable Here | `srtp_mode = "required"` but Twilio is offering plaintext — check the trunk's Secure Media toggle in the Twilio console |
| `200 OK` has `RTP/AVP` (not SAVP) | `srtp_mode = "off"` somewhere — check both `[media].srtp` and any `[route.media].srtp` override |
| Sngrep shows handshake but no payload | sngrep not built with libssl — switch to tcpdump for body inspection or accept the limitation |
| 403 Forbidden | Trunk allowlist doesn't cover Twilio's source IP — refresh edge IPs per `docs/TWILIO_INTEROP.md` |
| Silence on the call | Likely the bot side (WS server), not SRTP — try a known-working echo bot to rule out the SRTP layer |
