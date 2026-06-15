# Call Park & Retrieve

SiphonAI can **park** a call (0.7.0): shelve it without a WebSocket session,
play hold music to the caller, and keep the SIP dialog + RTP up until the
call is **retrieved** onto a *fresh* WS session — or it times out / hangs up.
Park detaches the bot from the call without ending the call.

```
                       park                          retrieve (operator)
WS server A ──────► (detached)        ... hold music ...    ──────► WS server B
   │                   │                                              │
caller ──RTP──► SiphonAI ─────── SIP dialog + RTP stay up ───────► SiphonAI ──► caller
```

The call's `MediaTap` task and SIP dialog are durable across a park; only the
WS bridge detaches. Retrieve opens a **new** session (new `seq` from 0,
`start.retrieved: true`, no replay) — it is **not** a mid-call WS reconnect.

## 1. Enabling

Park is **off by default** and fail-closed: a park is refused
(`error { code: "park_failed" }`) unless `[park]` is turned on. See
`docs/CONFIG.md` (`[park]`) for every field; the short version:

```toml
[park]
enabled        = true                      # false (default) = every park refused
moh_file       = "/etc/siphon-ai/moh.wav"  # optional; comfort noise if unset
timeout_secs   = 300                       # 0 = park indefinitely
timeout_action = "hangup"                  # "hangup" | "keep"
max_parked     = 32                        # daemon-wide cap (≥ 1)
```

`moh_file` is **validated and decoded at load** — a missing or garbage file
fails startup loud. Its native sample rate is resolved per-park; a call
negotiated at a *different* rate falls back to comfort noise (no resampling
in 0.7.0), logged once — not a park failure. Park is a daemon-level facility:
global only, no per-route overrides, and it applies to inbound **and**
outbound calls.

## 2. Parking a call

Two ways to park, both ending with SiphonAI sending `stop { reason: "park" }`
and closing that WS while the call lives on:

**WS server parks its own call** (self-scoped, `docs/PROTOCOL.md` §4.9):

```json
{ "type": "park", "call_id": "...", "slot": "lot-3" }
```

`slot` is an optional human label for the hold lot (surfaces in
`GET /admin/v1/parked` and the `call_parked` webhook). On success the server
receives `stop { reason: "park" }` and the WS closes — the server learns "you
were parked, not hung up." On failure (park disabled, or `max_parked`
reached) it gets `error { code: "park_failed" }` and the call continues
unparked on the current session.

**Operator parks any call** (admin API, `docs/DEPLOY.md`):

```sh
curl -X POST http://localhost:9091/admin/v1/calls/siphon-a/park \
    -d '{"slot":"lot-3"}'        # → 202 (dispatched); 404 unknown call
```

`202` means *dispatched* — the daemon signals the call and its own controller
parks it; the outcome surfaces on that call's WS (`stop{park}`) and the
`call_parked` webhook. A park refused by the `max_parked` cap is **not** a
`503` here — it surfaces as `park_failed` on the call's WS while the call
continues unparked.

## 3. Retrieving a parked call (operator-only)

Retrieve is driven **only** by the admin API — a parked call has no WS
session to ask, the mirror of why conference participants are removed by the
operator and not by a peer:

```sh
# ws_url is optional — defaults to the call's original bridge ws_url
curl -X POST http://localhost:9091/admin/v1/calls/siphon-a/retrieve \
    -d '{"ws_url":"wss://my-bot.example/retrieve"}'   # → 202
```

SiphonAI opens a **fresh** WS session to `ws_url` and sends a new `start` with
`retrieved: true` (`docs/PROTOCOL.md` §3.1) — the new server knows it's
picking up a parked call, not a fresh inbound one. Responses: `202`
dispatched, `404` unknown call, `409` if the named call exists but isn't
parked, `501` when park is off. Inspect what's parked with:

```sh
curl -s http://localhost:9091/admin/v1/parked
# → {"count":1,"parked":[{"call_id":"siphon-a","slot":"lot-3","parked_secs":42}]}
```

A call may park and retrieve repeatedly over its lifetime.

## 4. Timeout

`timeout_secs` bounds how long a call may stay parked (`0` = indefinite).
When it fires, SiphonAI emits the `park_timeout` webhook and then applies
`timeout_action`:

- **`hangup`** — tear the call down (normal teardown, CDR written).
- **`keep`** — leave it parked; the operator must retrieve or hang up. The
  timer does not re-arm.

A retrieve or a caller BYE before the deadline disarms the timer.

## 5. Observability

- **Metrics** — `siphon_ai_parks_total{result}` (`ok` / `rejected`),
  `siphon_ai_retrieves_total{result}` (`ok` / `not_parked`), and the
  `siphon_ai_parked_calls_active` gauge. See `docs/DEPLOY.md`.
- **Webhooks** — `call_parked { slot? }`, `call_retrieved { ws_url }`,
  `park_timeout { action }`. Payloads in `docs/DEPLOY.md`.
- **CDR** — `park { count, total_ms }` (additive, schema stays v1): park
  episodes + cumulative parked wall-time, omitted when the call was never
  parked.
- **Recording** — a recording in progress at park keeps writing; the parked
  span records the MOH the caller hears (consistent with "what the caller
  heard").
- **Logs** — `call parked` / `call retrieved onto fresh WS session` /
  `park timeout fired` at `info`, with `call_id` in the span.

## 6. Limitations (0.7.0)

- **Media-only park.** There is no SIP re-INVITE/hold — the dialog stays
  `sendrecv` and SiphonAI keeps sending RTP (the MOH needs it).
- **Retrieve is operator-initiated only**, onto a fresh WS session. This is
  deliberately **not** mid-call WS reconnect (still out of scope).
- **No resampling for MOH.** A `moh_file` whose native rate differs from the
  call's negotiated rate falls back to comfort noise.
- **No per-slot hold music.** The `slot` label is metadata only; every parked
  call hears the same `moh_file` (or comfort noise).
- **SIPp coverage asserts signaling + metrics**, not the MOH audio content:
  the park→retrieve→hangup and park→timeout→hangup phases verify the call
  survives a park and tears down on retrieve/timeout.
