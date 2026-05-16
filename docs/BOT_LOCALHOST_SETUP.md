# Running the Deepgram/OpenAI bot on the same host as SiphonAI

A focused walkthrough for putting the reference Node bot
(`examples/deepgram-openai-bot-node/`) on the same Debian 13 box
that runs the daemon. Bot listens on `127.0.0.1:8080`; daemon
connects to it over loopback.

This is the simplest topology — one VM, two services, no
network plumbing between them. Production deployments usually
move the bot to its own box; the steps below are the same, just
with a different `ws_url` and a firewall hole for the WS port.

Assumes you've followed `docs/INSTALL_DEBIAN13.md` already and
have a running `siphon-ai` service.

---

## 1. Install Node 20+

Debian 13's default `nodejs` package is older than the bot's
`@deepgram/sdk` v4 + `openai` v5 dependencies want. Use
NodeSource's repo for a current LTS:

```bash
curl -fsSL https://deb.nodesource.com/setup_22.x | sudo -E bash -
sudo apt install -y nodejs
node --version    # → v22.x
```

Or use `nvm` if you prefer. Anything Node 20+ works.

---

## 2. Install the bot

The bot ships in the repo you already cloned to
`/opt/siphon-ai-src`. Install its npm dependencies into the
example directory:

```bash
cd /opt/siphon-ai-src/examples/deepgram-openai-bot-node
npm install
```

The `package.json` pins `@deepgram/sdk`, `openai`, and `ws`.
`npm install` should finish in ~30 seconds. `package-lock.json`
gets created (gitignored — it's a per-host artifact).

---

## 3. Set the API keys

Both API keys live in environment variables. Put them in a file
the bot's systemd unit will read:

```bash
sudo install -d -o root -g root -m 0755 /etc/siphon-bot
sudo tee /etc/siphon-bot/env >/dev/null <<'EOF'
DEEPGRAM_API_KEY=dg_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx
OPENAI_API_KEY=sk-xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx
# Bind to loopback only — the daemon is on the same host.
BOT_BIND=127.0.0.1:8080
EOF
sudo chmod 0640 /etc/siphon-bot/env
```

Replace the `xxxxxx` placeholders. Set the file mode to `0640`
since it holds API credentials.

---

## 4. Smoke test in the foreground

Before wiring systemd, run it directly so you can see what it
prints on the first call. Two ways to feed the env file you
created in §3:

```bash
cd /opt/siphon-ai-src/examples/deepgram-openai-bot-node

# Option A — source the env file you wrote in §3.
set -a; sudo --preserve-env=PATH bash -c 'set -a; . /etc/siphon-bot/env; exec node server.js'

# Option B — inline the keys (handy for one-off testing).
# IMPORTANT: replace the literal ellipsis with your real keys.
# Pasting `…` verbatim sneaks a Unicode character past the env
# check; the bot bails with a clear "non-printable / non-ASCII
# characters" message rather than crashing inside the WS library.
DEEPGRAM_API_KEY='dg_real_key_here' \
OPENAI_API_KEY='sk-real_key_here' \
BOT_BIND=127.0.0.1:8080 \
node server.js
```

Expected output:

```
siphon-ai bot listening on ws://127.0.0.1:8080/
```

Leave it running. In another shell, place a test call from
FreeSWITCH (or whatever was triggering the failed call from your
log). The bot should now print something like:

```
[siphon-XXXX] START from=1001 to=9000 audio=pcm16le@8000Hz/20ms
[siphon-XXXX] STT open at 8000 Hz
[siphon-XXXX] TTS Flushed: NNN bytes
[siphon-XXXX] (interim): "..."
[siphon-XXXX] UTTERANCE: "..."
[siphon-XXXX] turn N start
[siphon-XXXX] LLM first token at +XXXms
[siphon-XXXX] first audio playing at +XXXms
…
```

And on the daemon side, `journalctl -u siphon-ai -f` should now
show `bridge connected` right after `state=Active` (the missing
log line in your original failure).

---

## 5. Update `[bridge].ws_url` to point at localhost

Your current daemon config probably has `ws_url = "ws://10.0.0.20:8080/"`
or similar from the install-guide example. With the bot on the
same host, point at loopback:

```bash
sudo sed -i 's|ws_url *= *"ws://[^/]*/"|ws_url = "ws://127.0.0.1:8080/"|' \
    /etc/siphon-ai/siphon-ai.toml
sudo systemctl restart siphon-ai
sudo journalctl -u siphon-ai -n 20 --no-pager
```

(Or edit the file by hand if you'd rather see the exact change.)

---

## 6. systemd unit

Put the bot under systemd so it survives reboots and restarts on
crash. The unit assumes the bot is running as your existing
`siphon` user (the operator account, not the `siphon-ai` service
account — the bot needs internet egress to Deepgram/OpenAI, and
keeping a separate `siphon-bot` user is overkill for one VM).

```bash
sudo tee /etc/systemd/system/siphon-bot.service >/dev/null <<'EOF'
[Unit]
Description=SiphonAI Deepgram/OpenAI voice agent
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=siphon
Group=siphon
WorkingDirectory=/opt/siphon-ai-src/examples/deepgram-openai-bot-node
EnvironmentFile=/etc/siphon-bot/env
ExecStart=/usr/bin/node server.js
Restart=always
RestartSec=5
StandardOutput=journal
StandardError=journal
LimitNOFILE=65536

[Install]
WantedBy=multi-user.target
EOF

sudo systemctl daemon-reload
sudo systemctl enable --now siphon-bot
sudo systemctl status siphon-bot --no-pager
```

Tail logs with `sudo journalctl -u siphon-bot -f`.

---

## 7. Verify end-to-end

Place a call from your softphone (registered as `1001` on
FreeSWITCH) to `9000`. Watch both services' journals at the
same time:

```bash
# terminal 1
sudo journalctl -u siphon-ai -f

# terminal 2
sudo journalctl -u siphon-bot -f
```

On the daemon side you should see the full lifecycle:

```text
INVITE routed route="fs-9000" from_user="1001" request_uri_user="9000" register_source="freeswitch-main"
inbound call media setup complete negotiated=PCMU sample_rate=8000 rtp_port=40222
call state state=Initializing
call state state=Connecting
call state state=Active
bridge connected         ← was missing before
… (call proceeds, lasts as long as the conversation)
call state state=Terminating
call ended cause=CallerHangup    ← or BridgeEnded, depending on who hung up
```

On the bot side you should see `START`, `STT open`, transcribed
fragments, and `turn N complete`.

---

## 8. Troubleshooting

### "BridgeEnded" arriving microseconds after "state=Active"

That's what your original log showed. The daemon's WS connect
fell over instantly. In order of likelihood:

| Cause | Check |
|---|---|
| Bot not running | `systemctl status siphon-bot` or `ss -tlnp \| grep :8080` |
| Wrong `ws_url` in daemon config | `grep ws_url /etc/siphon-ai/siphon-ai.toml`; loopback should be `ws://127.0.0.1:8080/` |
| Bot bound to a different interface | `BOT_BIND` must match — `127.0.0.1:8080` if daemon uses `ws://127.0.0.1:8080/`; `0.0.0.0:8080` if you want external WS too |
| Local firewall blocking loopback | Rare on Debian; check with `sudo nft list ruleset \| grep -i 8080` |

`journalctl -u siphon-ai | grep -E "bridge connected\|bridge connect"` —
absence of a `bridge connected` line per call is the diagnostic.

### Bot prints `refusing call: unsupported audio format`

The bot enforces `pcm16le / 20 ms / 8 kHz or 16 kHz` from the
`start` message. The daemon negotiates the format from the SIP
codec — PCMU produces 8 kHz, G.722 produces 16 kHz. Anything
else would mean a daemon-side bug; report it.

### Bot crashes on the first call with `Invalid Sec-WebSocket-Protocol value` or `invalid or duplicated subprotocol`

Stack ends in `new WebSocket` from either undici
(`node:internal/deps/undici/undici:…`) or `ws`
(`node_modules/ws/lib/websocket.js`), called from
`@deepgram/sdk/.../AbstractLiveClient.js` via the bot's
`openDeepgramStt`. Symptom on the daemon side is the same
fast-fail signature as a missing bot — `state=Active` →
`cause=BridgeEnded` in microseconds, no `bridge connected`
between them, because the bot died mid-handshake.

The Deepgram SDK's live client detects `globalThis.WebSocket` and
takes a code path that passes the API key as a subprotocol —
which both undici and `ws` reject as malformed (real API keys
contain characters that aren't valid in a `Sec-WebSocket-Protocol`
token). The bot's `server.js` deletes the global before requiring
the SDK so the SDK falls through to the `require('ws')` path with
`Authorization`-header auth, which works.

If you see the crash anyway, confirm the top of `server.js`
includes:

```js
delete globalThis.WebSocket;
const { WebSocketServer } = require('ws');
const { createClient } = require('@deepgram/sdk');
```

…in that order. The `delete` must run BEFORE the SDK loads.
Pulling the latest from `main` picks up the fix.

### Bot prints `TTS error` repeatedly

Usually a Deepgram API key issue — wrong key, expired, exceeded
quota. The bot drops the affected turn but keeps the call up so
the caller hears silence until they speak again. Verify with
`curl -H "Authorization: Token $DEEPGRAM_API_KEY" https://api.deepgram.com/v1/projects`.

### Audio one-way

Daemon hears the caller (STT transcripts appear in bot log) but
caller doesn't hear the bot. Almost always the RTP back-channel
to FreeSWITCH being blocked. Run `tcpdump -i any -n 'udp portrange
40000-40500'` on the SiphonAI host during a call — you should
see frames flowing both directions. If only inbound, FreeSWITCH's
side has a firewall rule dropping our RTP.

**Or** — if the FreeSWITCH dialplan is missing `bypass_media=true`,
FS's anchored bridge silently drops the SiphonAI→softphone
direction whenever the softphone offers `a=rtcp-mux` (most modern
softphones do, Zoiper included). See `docs/FREESWITCH_INTEGRATION.md`
§"Why bypass_media" for the full diagnosis. The fix is a one-line
dialplan addition; the rest of the system is fine.

### Bot speech cuts in and out every 1–2 seconds

Bot keeps interrupting itself mid-sentence. The log shows lots
of `barge-in: dropping playout + sending clear` lines back-to-back.

This is acoustic feedback: caller is on speakerphone with the
speaker right next to the mic, the bot's voice plays through the
speaker, the mic picks it up, the daemon's VAD says
`speech_started`, and the bot's `speech_started` handler cancels
its own playout. The bot is barging in on itself.

Options:

1. **Use a headset on the softphone.** Eliminates the acoustic
   loop instantly. This is the right answer for any production
   caller (real phones have hardware AEC).
2. **Switch the daemon's barge-in mode to `notify_only`.** The
   daemon still emits `speech_started` to the bot but doesn't
   auto-flush its playout buffer. The reference bot still does
   its own cancel, so for a fully-tolerant demo you'd also want
   to comment out the `playout.cancel()` line in the bot's
   `speech_started` handler. In `/etc/siphon-ai/siphon-ai.toml`:
   ```toml
   [bridge.barge_in]
   mode = "notify_only"
   ```
   Then `sudo systemctl restart siphon-ai`.

Don't try to "fix" this in production by raising the VAD
threshold — real callers' phones have AEC and the chop never
appears. It's purely an artifact of the speakerphone test setup.

---

## What this skips

- **TLS for the WS.** Bot listens on `ws://`, not `wss://`. Fine
  for loopback. For inter-host WS use `wss` + a cert; SiphonAI's
  bridge supports `wss://` URLs natively.
- **Bot auth.** The daemon's `[route.bridge].ws_auth_header` can
  inject an `Authorization` header on the upgrade; the bot in
  this example doesn't check it. Add a check in `handleCall` if
  you want.
- **Multiple bots / load-balancing.** One bot serves all calls;
  fine for small deployments. For scale, run several behind a
  WS-aware load balancer (e.g., HAProxy) and point `ws_url` at
  the VIP.
