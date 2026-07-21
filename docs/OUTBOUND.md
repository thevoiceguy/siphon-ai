# Outbound Call Origination

SiphonAI can **place** calls, not just answer them (0.6.0). An HTTP request
names a destination and a gateway; SiphonAI sends the INVITE, and when the
callee answers it bridges the call's audio to your WebSocket server over the
same protocol inbound calls use. Callbacks, reminders, notifications,
outbound bots — same WS server, inverted call direction.

```
POST /admin/v1/calls ──► SiphonAI ──INVITE──► gateway/trunk ──► callee
                            │                                     │
                            ◄────────────── 200 OK (answer) ──────┘
                            │
                            └──WebSocket──► your WS server (same protocol v1)
```

**Outbound calls spend money.** Read [§3](#3-security--toll-fraud) before
exposing anything.

## 1. Enabling

Outbound is **off by default** and fail-closed: it activates only when
`[outbound].max_concurrent` is a positive number and at least one
`[[gateway]]` is configured. See `docs/CONFIG.md` (`[outbound]` +
`[[gateway]]`) for every field; the short version:

```toml
[outbound]
max_concurrent     = 20      # 0 (default) = outbound disabled
rate_limit_per_sec = 5       # optional new-calls/sec ceiling

# A standalone trunk (static / IP-auth or digest):
[[gateway]]
name          = "twilio"
proxy         = "siptrunk.example.com:5060"
from          = "sip:+13125551234@siptrunk.example.com"
auth_username = "ACxxxx"                # optional digest
auth_password = "${TWILIO_TRUNK_SECRET}"

# Or dial through an existing [[register]] (reuse server + credentials):
[[gateway]]
name     = "pbx-out"
register = "pbx"

# The originate endpoint lives on the dedicated [admin] listener (0.10.0),
# NOT on [observability].http_listen — that port returns 404 for /admin/*.
[admin]
listen = "127.0.0.1:9092"

# Origination is billable, so it requires an `admin`-role token.
[[admin.token]]
name  = "automation"
token = "${SIPHON_ADMIN_ADMIN}"
role  = "admin"
```

The originate endpoint is part of the admin API, so **`[admin]` must be
configured with at least one `admin`-role token** — omit `[admin]` entirely
and `/admin/*` isn't served at all. `[observability]` is unrelated to
origination. See `docs/DEPLOY.md` → Admin auth & RBAC for the full listener,
role, and `[admin.tls]` reference. Gateway config is validated at startup —
bad `proxy`, unknown `register` reference, or half-set credentials fail loud.

## 2. Placing a call

```sh
ADMIN=http://127.0.0.1:9092          # https://… when [admin.tls] is set

curl -X POST $ADMIN/admin/v1/calls \
    -H "Authorization: Bearer $SIPHON_ADMIN_ADMIN" \
    -d '{
  "to": "+15558675309",
  "gateway": "twilio",
  "ws_url": "wss://my-bot.example/outbound"
}'
# → 202 {"call_id":"siphon-…"}
```

Origination needs an **`admin`**-role token (the highest role — it's the
billable endpoint). A missing or invalid token is `401`; a `readonly` or
`operator` token is `403`.

| Field | Required | Notes |
|---|---|---|
| `to` | yes | Destination (E.164 / SIP user). Becomes the Request-URI user at the gateway's proxy. |
| `gateway` | yes | A `[[gateway]]` name. `404` if unknown. |
| `ws_url` | no | WS server for this call. Falls back to `[bridge].ws_url`; `400` if neither is set. |
| `from` | no | Caller-ID override (full `sip:` URI) — becomes the INVITE **From** header the trunk validates, so it must be a number your provider accepts (an owned/verified caller-ID). Falls back to the gateway's `from`. A malformed value is rejected `400`. |
| `delayed_offer` | no | Place the call as a delayed offer (RFC 3264, 0.9.0): INVITE without SDP, answer the peer's offer in the ACK. Default `false`. |
| `recording` | no | Recording override for this leg (0.26.0): `"off"` / `"always"` / `"on_demand"`. Falls back to the gateway's `recording` default (itself `"off"`). `400` for other values or when recording is requested with no `[recording].dir` configured. Recorded outbound legs behave exactly like inbound: same dir/encryption/format/upload, `recording_*` on the CDR, on-demand WS controls. |

`202` means *admitted and dialing*, not answered — the HTTP exchange ends
there, and everything after arrives out-of-band (see [§4](#4-call-lifecycle)).
Other responses: `401` missing/invalid token, `403` token below `admin`
role, `404` unknown gateway, `400` bad target / no ws_url / invalid JSON,
`503` at `max_concurrent`, `429` rate-limited, `501` outbound disabled.

Digest auth (401/407 challenges from the trunk) is answered automatically
with the gateway's credentials; there's nothing per-call to do.

## 3. Security — toll fraud

This is the first SiphonAI feature where a compromised or misconfigured
deployment **directly costs money**: anyone who can reach the originate
endpoint can place calls billed to your trunk. Premium-rate fraud burns
thousands of dollars in hours, so treat the endpoint like a payment API.

**The originate API requires an `admin`-role bearer token** (0.10.0; the
0.6.0 posture of "no built-in auth, front it with a proxy" is obsolete —
see `docs/design/DEV_PLAN_0.6.0.md` §9.5 for the original decision and
`docs/DEPLOY.md` → Admin auth & RBAC for what replaced it). The posture:

1. **Give origination its own `admin`-role token**, separate from the
   `readonly` / `operator` tokens used for dashboards and routine ops. It's
   the highest role precisely because it spends money; nothing that only
   needs to *read* state should hold a token that can dial.
2. **Keep `[admin].listen` on loopback or a private interface**
   (`127.0.0.1:9092`), and set `[admin.tls]` if it must bind somewhere
   routable — the bearer token is plaintext on the wire otherwise. Never
   expose the admin port raw to the internet; the token is a credential,
   not a substitute for network placement.
3. **Rotate by editing `[admin.token]` and restarting.** Token changes are
   *not* picked up by SIGHUP reload — a revoked token keeps working until
   restart (`docs/DEPLOY.md`).
4. **Set a realistic `max_concurrent`.** It's the blast-radius cap; 20
   concurrent premium-rate calls is a very different incident than 2000.
5. **Set `rate_limit_per_sec`.** A dialer bug or a stolen `curl` loop hits
   the token bucket instead of your trunk.
6. **Use trunk-side allowlists too.** Most providers can restrict
   destinations (countries, premium ranges) per trunk — defense in depth
   that survives a SiphonAI misconfiguration.
7. **Watch `siphon_ai_outbound_calls_total`.** An unexpected slope on that
   counter is the earliest fraud signal you'll get; alert on it. Pair it
   with `siphon_ai_admin_requests_total{endpoint,role,result}` — a rising
   `unauthenticated` / `forbidden` count on the originate endpoint is
   someone probing it.

### Encrypting the media — SRTP (0.7.x)

By default outbound media is plaintext RTP. To secure it, set
`[[gateway]].srtp` (the outbound mirror of inbound `[media].srtp`):

```toml
[[gateway]]
name      = "twilio"
proxy     = "siptrunk.example.com"
from      = "sip:+13125551234@siptrunk.example.com"
transport = "tls"          # secures the signalling that carries the SDES key
srtp      = "required"      # "off" (default) | "preferred" | "required"
recording = "off"           # per-gateway recording default (0.26.0):
                            # "off" | "always" | "on_demand"
```

SiphonAI offers SDES (RFC 4568): it mints an `AES_CM_128_HMAC_SHA1_80` master
key, sends the INVITE as `RTP/SAVP` with an `a=crypto:` line, and on a 2xx
that accepts it installs the keys so the trunk leg is encrypted. The mode
controls the downgrade:

- **`required`** — a trunk that answers plaintext `RTP/AVP` **fails the
  call** (it counts as `failed` on `siphon_ai_outbound_calls_total`). Use
  this when the carrier mandates SRTP (e.g. Twilio secure trunking).
- **`preferred`** — a plaintext answer is accepted and the call continues
  **unencrypted** (best-effort). `start.srtp` is then absent.

**Always pair `srtp` with `transport = "tls"`.** SDES carries the master key
in the SDP on the signalling plane; over plaintext SIP the key is exposed
and SRTP gives no real confidentiality. The daemon warns at load if a
gateway sets `srtp` without TLS. When SRTP is established, the WS server
sees it on `start.srtp` (`{ exchange: "sdes", profile: "<suite>" }`, see
`docs/PROTOCOL.md` §3.1), and the `siphon_ai_outbound_srtp_total{result}`
metric records `encrypted` vs `downgraded`.

## 4. Call lifecycle

Progress is reported via lifecycle webhooks (`[webhooks]`, see
`docs/DEPLOY.md`), all carrying the `call_id` the originate request
returned:

```
outbound_initiated ──► outbound_answered ──► call_end   (+ CDR)
        │
        └────────────► outbound_failed                  (terminal, no CDR)
```

- **`outbound_initiated`** `{to, gateway}` — admitted, INVITE going out.
- **`outbound_answered`** `{sip_call_id}` — callee sent 2xx; media is bound
  and the WS bridge is connecting.
- **`outbound_failed`** `{cause}` — ended without answer: `busy` /
  `declined` / `no_answer` / `rejected` / `unreachable` / `failed` (same
  strings as the metric labels).
- **`call_end`** — same shape as inbound; `route` carries the gateway name.

Answered calls also produce a CDR with `direction: "outbound"` (schema
stays v1; `route` = gateway name, `started_at` = INVITE dispatch so
`duration_ms` includes ring time). Unanswered calls get **no CDR** — the
`outbound_failed` webhook + metric cover them, mirroring inbound where CDRs
cover bridged calls only.

### What your WS server sees

The same protocol v1 session as inbound, with `start.direction:
"outbound"` (additive — servers that ignore it keep working). `from` is the
caller-ID the call was placed with, `to` the dialed destination. The
`start` arrives **after answer** — by the time your server gets it, a human
(or their voicemail) is already listening, so speak first: an outbound bot
should send audio immediately, not wait for caller speech like an inbound
greeting flow might. Everything else — audio frames, barge-in, DTMF,
`hangup`, transfer — behaves identically to inbound. See
`docs/PROTOCOL.md`.

### Consult legs — attended transfer (0.6.1)

An outbound call doubles as the **consult leg** of an attended transfer:
place it with `POST /admin/v1/calls`, let the bot talk to the consulted
party over that call's own WS session, then send
`transfer { replaces_call_id: "<the consult call_id>" }` on the *original*
call's session. SiphonAI REFERs the original peer with a `Refer-To` that
embeds a `Replaces` built from the consult dialog, so the two humans
connect directly and both SiphonAI legs end. The consult call must be
**answered** when the transfer fires, and SiphonAI does not tear it down at
REFER time — the transferee's INVITE-with-Replaces takes it over. Field
semantics, target derivation/override, and error cases are in
`docs/PROTOCOL.md` §4.4. Outbound legs are themselves transferable too
(blind or attended), so an outbound bot can hand its callee off the same
way.

## 5. Observability

- **Metrics** — `siphon_ai_outbound_calls_total{result}` (`answered`,
  `busy`, `declined`, `no_answer`, `rejected`, `unreachable`, `failed`) and
  the `siphon_ai_outbound_calls_active` gauge (admitted but not yet
  settled). When a gateway uses SRTP, `siphon_ai_outbound_srtp_total{result}`
  records `encrypted` vs `downgraded` (0.7.x). Bridged-call mechanics land
  in the same per-call metrics inbound calls use.
- **CDR** — `direction: "outbound"`, see [§4](#4-call-lifecycle) and
  `docs/DEPLOY.md` (CDR consumers).
- **Webhooks** — the three `outbound_*` events + `call_end`, payloads in
  `docs/DEPLOY.md` (lifecycle webhooks).
- **Logs** — `originating outbound call` / `outbound call answered` /
  `outbound call ended` at `info`, all with `call_id` in the span.
- **HEP/Homer** — the outbound INVITE/BYE transactions ship via the same
  siphon-rs HEP emission as inbound; correlate by the SIP Call-ID from
  `outbound_answered.sip_call_id` or the CDR.

## 6. Testing without spending money

The SIPp regression suite has an always-on outbound phase
(`test-harness/sipp-scenarios/outbound_uas_answer.xml`): SIPp plays the
callee, a throwaway daemon dials it through a loopback gateway, and the
echo WS server ends the call. The same trick works interactively — point a
`[[gateway]]` at any lab UAS (`proxy = "127.0.0.1:5080"`) and originate
against it; nothing leaves the machine.

## 7. Limitations (v0.6.1)

- **No early media** — audio before the 200 OK (ringback injected by the
  far end, IVR pre-answer prompts) is not bridged; the WS session starts at
  answer. Planned as a stretch follow-up.
- **No mid-call progress webhook for ringing** — `outbound_initiated` fires
  at INVITE, the next signal is answered/failed. (180 Ringing is visible in
  HEP/Homer if you need it.)
- **Recording** is not wired for outbound calls in this release.
- **No STIR/SHAKEN signing** — SiphonAI verifies inbound `Identity` headers
  but does not sign outbound INVITEs; attestation for your calls is the
  trunk provider's job.
- **AMD (answering-machine detection)** is not built in — your WS server
  hears the answered audio and can run its own detection (that's the
  provider-neutral hook; see CLAUDE.md §4.1).
