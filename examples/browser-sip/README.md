# browser-sip — SIP.js in a real browser against siphon-ai

The hands-on exit check for **Phase 1 of
[`docs/design/DEV_PLAN_WebRTC.md`](../../docs/design/DEV_PLAN_WebRTC.md)**:
SIP.js in Chrome connects over WSS, REGISTERs with digest credentials,
and places a test call — against the real daemon, no simulators.

**What passes Phase 1 here is REGISTER.** It exercises the whole new
surface end-to-end: the `[sip.wss]` listener, the `Origin` allow-list,
`sip` subprotocol negotiation, digest auth with AOR authorization, the
`[registrar]` binding, and expiry when the tab closes. The **test call
is signaling-only**: the browser offers WebRTC media (ICE/DTLS-SRTP)
that the daemon cannot terminate until Phase 2's `webrtc-glue`, so
expect no audio and treat the call's outcome as diagnostic — the
INVITE arriving, routing, and showing up in the daemon log (and Homer)
is the point.

## The five minutes

From the repo root:

```bash
# 1. Certificate (mkcert if you have it — zero browser prompts;
#    otherwise openssl self-signed + one trust click, see below)
./examples/browser-sip/gen-cert.sh

# 2. Terminal A — the echo WS server (the bridge's AI side)
python examples/echo-ws-server-python/server.py --bind 127.0.0.1:8765

# 3. Terminal B — the daemon
cargo run -p siphon-ai -- --config examples/browser-sip/lab.toml

# 4. Terminal C — serve the test page (file:// won't do: the daemon's
#    Origin allow-list expects http://127.0.0.1:8088)
python3 -m http.server 8088 --bind 127.0.0.1 --directory examples/browser-sip
```

5. Open <http://127.0.0.1:8088>, click **Connect & Register**.

### If gen-cert.sh fell back to openssl

Browsers won't show a trust prompt for a WebSocket, so grant the
exception once via a regular tab: with the daemon running, open
<https://127.0.0.1:8443/> and click through the warning (Advanced →
Proceed). The page errors — fine; the recorded exception is what lets
`wss://127.0.0.1:8443` connect. (`mkcert` makes this step disappear.)

## What you should see

- The page turns **registered**, and logs
  `REGISTER accepted — Phase 1 exit check: PASS`.
- Daemon log: `registration bound aor=sip:browser@127.0.0.1 via_stream=true`.
- `curl -s http://127.0.0.1:9091/metrics | grep registrar` →
  `siphon_ai_registrar_bindings 1` and
  `siphon_ai_registrar_registers_total{result="ok"} 1` (plus one
  `challenged` — the normal digest first leg).
- **Close the tab**: the binding expires ~32 s later
  (`registration expired (connection lost)` in the daemon log, gauge
  back to 0). A reload inside that window re-REGISTERs and survives.
- Wrong-origin check: open the page from `http://localhost:8088`
  *after removing* that entry from `allowed_origins` in `lab.toml` —
  the upgrade is refused 403.

## No display / no Chrome on the box?

```bash
./examples/browser-sip/headless-check.sh
```

runs the whole thing self-driving: boots the lab stack, drives the
page with headless Chromium (system `chromium` if installed, else
Playwright's download — **no root needed**: the two NSS libraries a
server install lacks are fetched with `apt-get download` and injected
via `LD_LIBRARY_PATH`), asserts `siphon_ai_registrar_bindings` goes to
1, kills the browser, and asserts the connection-loss expiry brings it
back to 0. Prints `PASS` on a full run. Needs `cargo build -p
siphon-ai` first, plus `node`/`npx` and network for the Playwright
fallback. This is also the seed of the plan's Phase 3 nightly
browser-e2e harness.

The page's `?auto=1` mode is what the script drives (auto-register,
result breadcrumbs on the console); `?auto=1&call=1` additionally
fires the test call — with today's daemon that ends in the precise
`488` (`offer profile UDP/TLS/RTP/SAVPF rejected`) that marks exactly
where Phase 2's WebRTC media leg plugs in.

## Homer (the "fully visible in Homer" criterion)

```bash
docker compose -f examples/homer-stack/compose.yaml up -d
```

then uncomment the `[hep]` block in `lab.toml` and restart the daemon.
Every WS-carried SIP message (REGISTER, 401, the authenticated
REGISTER, the test-call INVITE) appears in Homer correlated by Call-ID
— HEP capture on the WS transport is the same wiring as every other
transport. See [`docs/HEP.md`](../../docs/HEP.md).

## Files

| File | What |
|---|---|
| `index.html` | The SIP.js page (SIP.js 0.21 from unpkg; needs network on first load). |
| `lab.toml` | Daemon config: `[sip.wss]` + `[sip.auth]` + `[registrar]`, Origin allow-list pinned to the page's origin. |
| `gen-cert.sh` | mkcert-or-openssl certificate for `localhost`/`127.0.0.1` into `certs/`. |

Credentials are `browser` / `s3cret-ws` (realm `siphon.example`) —
matching `lab.toml`; change both together.
