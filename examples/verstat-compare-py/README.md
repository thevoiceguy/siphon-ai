# verstat-compare — cross-check SiphonAI vs Twilio's `X-Twilio-VerStat`

A diagnostic WebSocket server that compares two independent signals about
the same inbound call:

- **ours** — `start.verstat`, where SiphonAI *verified* the PASSporT itself
  (x5u fetch, chain to the STI-PA anchor, ES256 signature, orig/dest TN
  binding, `iat` freshness), and
- **Twilio's** — the `X-Twilio-VerStat` SIP header Twilio sets on inbound
  calls, forwarded to the WS server via `[bridge].forward_headers`.

It logs `AGREE` / `DIVERGE` per call and plays no audio. Use it during
initial STIR/SHAKEN rollout to confirm your verification matches what the
carrier already told you — and to catch misconfiguration (a stale trust
anchor that fails every call Twilio says passed shows up as a wall of
`DIVERGE`).

## Run

```bash
python3 -m venv .venv && .venv/bin/pip install websockets
.venv/bin/python server.py --bind 127.0.0.1:8765
```

Point SiphonAI at it and make sure the daemon both verifies and forwards
Twilio's header:

```toml
[bridge]
ws_url = "ws://127.0.0.1:8765/"
forward_headers = ["X-Twilio-VerStat"]   # surface Twilio's claim on `start`

[security]
min_attestation = "none"                 # observe only — don't gate yet

[security.stir_shaken]
enabled       = true
trust_anchors = "/etc/siphon-ai/sti-pa-roots.pem"
```

`min_attestation = "none"` keeps it observe-only so a divergence doesn't
reject calls while you're still validating the setup.

## What the log lines mean

- `AGREE` — our verdict and Twilio's match (same pass/fail, same
  attestation when passed). Expected for healthy US-originated calls.
- `DIVERGE` — they disagree. Common causes: a stale/missing `trust_anchors`
  bundle (we fail what Twilio passed), a clock skew tripping `iat`
  freshness, or a genuinely interesting call. Investigate before turning on
  a `min_attestation` gate.
- "no SiphonAI verstat" — `[security.stir_shaken].enabled` is false.
- "no X-Twilio-VerStat" — not a Twilio call, or the header isn't in
  `[bridge].forward_headers`.

See [`docs/INTEGRATIONS_TWILIO.md`](../../docs/INTEGRATIONS_TWILIO.md) for
the full recipe and [`docs/SECURITY_STIR_SHAKEN.md`](../../docs/SECURITY_STIR_SHAKEN.md)
for what attestation does and doesn't tell you.
