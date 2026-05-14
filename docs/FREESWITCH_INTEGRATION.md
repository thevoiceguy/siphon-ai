# FreeSWITCH → SiphonAI Trunk Integration

End-to-end recipe for a registered FreeSWITCH softphone (extension
`1001`) that dials `9000` and reaches a SiphonAI-bridged bot via
a SIP trunk between the two systems.

```
┌────────────┐ INVITE 1001@fs    ┌────────────┐ INVITE 9000@siphon ┌────────────┐ ws://
│  softphone │ ─────registered──▶│ FreeSWITCH │ ─────SIP trunk────▶│  SiphonAI  │ ───────▶ bot
│  (1001)    │                   │            │                    │            │
└────────────┘                   └────────────┘ ◀─────RTP─────────▶└────────────┘
```

Two assumptions:

- FreeSWITCH and SiphonAI are on separate hosts on the same
  network (or one VLAN apart with UDP 5060 + the RTP range
  reachable in both directions).
- FreeSWITCH already has a working `internal` profile where
  extension `1001` is registered and can place internal calls.
  This guide adds the trunk leg.

Replace IPs and credentials with yours throughout. The IPs below:

- `10.0.0.10` — FreeSWITCH
- `10.0.0.20` — SiphonAI daemon
- `10.0.0.30` — Node bot (WS server on port 8080)

---

## 1. SiphonAI side

### Config

Use the `[sip]` / `[bridge]` / `[[route]]` shape from
`docs/INSTALL_DEBIAN13.md` §5. The route can be a simple
catch-all, or you can be explicit and match only `9000`:

```toml
[sip]
listen     = "0.0.0.0:5060"
transports = ["udp"]

[media]
codecs                  = ["pcmu"]      # FreeSWITCH default. PCMA works too.
rtp_port_range          = [40000, 40500]
inactivity_timeout_secs = 60

[bridge]
ws_url                = "ws://10.0.0.30:8080/"
ws_connect_timeout_ms = 3000

# Trunk allowlist — INVITEs from any other peer get 403.
# Required for production. See docs/CONFIG.md §"[[trunk]]" for
# the threat model.
[[trunk]]
name       = "freeswitch-main"
peer_addrs = ["10.0.0.10"]              # FreeSWITCH server

[[route]]
name = "freeswitch-9000"
[route.match]
register_source = "freeswitch-main"     # only accept from this trunk
request_uri_user = "9000"
[route.bridge]
# Per-route override: an auth header if the bot wants it.
ws_auth_header = "Bearer ${BOT_TOKEN}"

[[route]]
name = "default"
[route.match]
register_source = "freeswitch-main"     # still scoped to the trunk
any             = true
```

The trunk gate runs *before* route matching, so by the time a
route is consulted the INVITE has already been authenticated as
coming from `freeswitch-main`. Scoping each route by
`register_source` isn't required for security — it's clarity:
operators looking at the dialplan can see which routes belong
to which trunk without having to cross-reference the gate.

`BOT_TOKEN` lives in `/etc/siphon-ai/env` (see install guide
§5). Apply with `sudo systemctl restart siphon-ai`.

### Verify the daemon is reachable from FreeSWITCH

From the FreeSWITCH box:

```bash
nc -zvu 10.0.0.20 5060   # UDP — `nc` will report "open" on most distros
sipp -sn uac 10.0.0.20:5060 -m 1 -s 9000
# → SIP/2.0 200 OK
```

If SIPp times out, the firewall rule from the install guide §7
isn't accepting FreeSWITCH's source IP — fix and retry.

---

## 2. FreeSWITCH side

### Gateway (peer trunk, no REGISTER)

SiphonAI's `[[trunk]]` allowlist (above) handles peer
identification by IP. SIP-digest auth on inbound INVITEs is
post-v1, so the FreeSWITCH gateway runs in "peer mode": no
`register`, no `username`/`password`. Put this in
`/etc/freeswitch/sip_profiles/external/siphon-ai.xml`:

```xml
<include>
  <gateway name="siphon-ai">
    <param name="proxy"          value="10.0.0.20:5060"/>
    <param name="register"       value="false"/>
    <param name="ping"           value="30"/>
    <!-- Don't rewrite Contact; SiphonAI uses what we send. -->
    <param name="extension-in-contact" value="false"/>
    <!-- PCMU only, matching the daemon's [media].codecs. -->
    <param name="caller-id-in-from" value="true"/>
  </gateway>
</include>
```

Codec narrowing happens in dialplan with `absolute_codec_string`
(below). Pick one or the other — having both is fine but the
dialplan one wins.

### Dialplan: route 9000 → SiphonAI gateway

In `/etc/freeswitch/dialplan/default/99_siphon_ai.xml`:

```xml
<include>
  <extension name="siphon-ai-9000">
    <condition field="destination_number" expression="^9000$">
      <!-- Force PCMU on the trunk leg so SiphonAI doesn't have
           to deal with re-negotiation. -->
      <action application="set" data="absolute_codec_string=PCMU"/>
      <!-- Some FS deployments default to inactive Contact rewrites
           that cause SDP loops with strict UASes; turn off. -->
      <action application="set" data="hangup_after_bridge=true"/>
      <!-- Pass the SIP `From` user through so the bot sees the
           extension that placed the call in start.from. -->
      <action application="set" data="effective_caller_id_number=${caller_id_number}"/>
      <action application="set" data="effective_caller_id_name=${caller_id_name}"/>
      <!-- Bridge to the SiphonAI gateway. Request-URI user = 9000
           so the daemon's per-route match key fires. -->
      <action application="bridge" data="sofia/gateway/siphon-ai/9000@10.0.0.20"/>
    </condition>
  </extension>
</include>
```

Reload:

```bash
fs_cli -x "reloadxml"
fs_cli -x "sofia profile external rescan"
fs_cli -x "sofia status gateway siphon-ai"
# → status RUNNING, ping 0/30, no failure counters
```

`ping 30` is FreeSWITCH's OPTIONS keepalive. SiphonAI doesn't
auto-respond to OPTIONS in v1 — that's harmless (FreeSWITCH
keeps the gateway UP regardless of pong loss) but it'll show
non-zero `FAIL` counters on `sofia status`. The dial-out path
isn't affected.

### Optional: firewall ACL for inbound RTP

If the FreeSWITCH host firewall blocks the SiphonAI RTP range,
audio one-way's you. Allow:

```bash
# From SiphonAI to FreeSWITCH's external RTP range
sudo nft add rule inet fs_rules input udp dport 16384-32768 ip saddr 10.0.0.20 accept
```

(FreeSWITCH's default RTP range is `16384-32768`.)

---

## 3. End-to-end test

Place a call from your softphone (registered as `1001` to
FreeSWITCH) to `9000`:

1. **Softphone** dials 9000.
2. **FreeSWITCH** routes the destination → bridges via the
   `siphon-ai` gateway → INVITE flows to `10.0.0.20:5060`.
3. **SiphonAI** matches the `9000` route → opens WS to
   `ws://10.0.0.30:8080/`.
4. **Bot** sends a `start` ack (none required in protocol; bot
   just starts receiving binary frames + sending its own).
5. **Audio** flows: caller's voice → SiphonAI → WS binary frames
   to bot; bot's TTS → WS binary frames → SiphonAI → RTP to
   FreeSWITCH → softphone.

### What to watch

**SiphonAI metrics:**

```bash
curl -s http://127.0.0.1:9091/metrics | grep -E '^siphon_ai_(invites_total|calls_active|route_match_total)'
```

`siphon_ai_route_match_total{route="freeswitch-9000"}` should
tick once per call. `siphon_ai_calls_active` should be `1` during
the call and drop to `0` on hangup.

**FreeSWITCH:**

```bash
fs_cli -x "show calls"
fs_cli -x "sofia status gateway siphon-ai"
```

If the call rings but goes silent immediately, run `tcpdump` on
the RTP range on the SiphonAI side — usually a firewall blocking
RTP back to FreeSWITCH or a `c=` line advertising the wrong IP
(check `[node].public_address` in the daemon's config).

### Common interop gotchas

- **No audio one direction**: the side missing audio has a firewall
  blocking the other side's RTP. Test with `tcpdump -i any -n
  udp portrange 40000-40500`.
- **One-way then full audio**: symmetric-RTP latching kicked in
  after the first inbound frame. Acceptable, but it means the
  peer changed RTP endpoints — PR #26 wires
  `update_participant_media` so this should no longer happen on
  re-INVITE; if it does on initial INVITE, check `[node].public_address`.
- **488 Not Acceptable Here**: FreeSWITCH and SiphonAI's `[media].codecs`
  don't intersect. With `absolute_codec_string=PCMU` and
  `codecs = ["pcmu"]` they will.
- **No `start` event reaches the bot**: SiphonAI couldn't connect
  to `ws_url`. Check `journalctl -u siphon-ai -f` for
  `bridge connected` vs a connect error.

---

## 4. Building the bot

See `examples/deepgram-openai-bot-node/` for a working bot that
implements the SiphonAI bridge protocol. It uses Deepgram for
STT/TTS and OpenAI for the LLM, and (unlike the FreeSWITCH
`audio_fork` model) sends TTS audio back as PCM16 frames over
the same WebSocket — no `uuid_broadcast` or ESL needed.
