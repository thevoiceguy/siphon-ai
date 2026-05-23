# Twilio Elastic SIP Trunking — minimal siphon-ai config

This directory has the bare minimum to bridge Twilio inbound calls
into a WebSocket server through siphon-ai. It's the runnable form of
the recipe in [`docs/INTEGRATIONS_TWILIO.md`](../../docs/INTEGRATIONS_TWILIO.md)
— that doc explains what each line means and walks the Twilio-side
setup; this one is just the file you'd drop into `/etc/siphon-ai/`
and edit.

## Files

| File             | Purpose                                                  |
| ---------------- | -------------------------------------------------------- |
| `siphon-ai.toml` | The whole config. Edit the marked placeholders.          |
| `README.md`      | This file.                                               |

## What to edit before running

1. `[node].public_address` — the IP / DNS name Twilio dials.
2. `[[trunk]].sources` — replace the RFC-5737 placeholders with
   Twilio's actual signalling IPs for the regions you accept calls
   from. Twilio's [SIP signaling IP addresses][twilio-ips] page has
   the current list.
3. `[bridge].ws_url` — point at your own WebSocket server, or leave
   it pointing at the echo-ws-server example for a first-call test.

## End-to-end smoke test

```bash
# Terminal 1 — echo WS server
cd ../echo-ws-server-python
python3 -m venv .venv && .venv/bin/pip install -r requirements.txt
.venv/bin/python server.py --bind 127.0.0.1:8765

# Terminal 2 — siphon-ai
cargo run -p siphon-ai -- --config examples/twilio-trunk/siphon-ai.toml
```

Then place a call to your Twilio number. The caller hears their
own audio echoed back (the echo server is a transport-layer
smoke test; swap in a real STT/LLM/TTS WS server for an actual
voice agent).

## What this example does NOT cover

- **TLS / SIPS.** Production deployments should use SIP over TLS;
  see `docs/CONFIG.md` §`[sip.tls]` and the deployment recipe in
  `docs/DEPLOY.md`.
- **The Programmable Voice `<Dial><Sip>` flow.** Where you route
  calls through TwiML instead of an Elastic Trunk. Sketched in
  `docs/INTEGRATIONS_TWILIO.md` §"Programmable Voice alternative"
  with a TwiML snippet; not duplicated as a runnable example
  because the siphon-ai side is identical to this one.

[twilio-ips]: https://www.twilio.com/docs/sip-trunking#ip-addresses
