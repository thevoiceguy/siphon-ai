# SiphonAI WebSocket Bridge Protocol — v1

This is the public API of SiphonAI: the contract a developer's WebSocket
server speaks to bridge a SIP call's audio. Treat it like a published
API — see `CLAUDE.md` §4.2 before changing anything here.

> **Status:** v1. Breaking changes require a `version` bump on the `start`
> message and a new section in this document. Additive, backward-compatible
> changes (new optional fields, new `error.code` values) do not bump the
> version but MUST be documented in the same PR.

---

## 1. Connection model

| Property | Value |
|---|---|
| Direction | SiphonAI is the WebSocket **client**; the developer's server is the **WebSocket server**. |
| Cardinality | **One WebSocket connection per call.** All frames on a connection belong to the same call. |
| URL | `bridge.ws_url` from SiphonAI's TOML config. May be overridden per route via `routes.*.bridge.ws_url`. |
| Scheme | `ws://` and `wss://`. `wss://` is recommended for any non-loopback deployment. |
| Subprotocol | SiphonAI sends `Sec-WebSocket-Protocol: siphon-ai.v1`. Servers SHOULD echo it; SiphonAI proceeds even if the server doesn't, so simple/dumb servers work. |
| Auth | If `bridge.auth_bearer` is configured, SiphonAI sends `Authorization: Bearer <token>` on the upgrade request. |
| Headers | Servers may inspect any HTTP header on the upgrade request. SiphonAI sets `User-Agent: siphon-ai/<version>` and forwards a `X-Siphon-Call-Id: <call_id>` header for log correlation. When `[observability.otlp]` is enabled (0.23.0), SiphonAI also sends W3C [`traceparent`](https://www.w3.org/TR/trace-context/) (+ `tracestate` when non-empty) so the server can continue the call's distributed trace; the same values are mirrored on `start.trace_context` for servers whose WS library hides upgrade headers. Absent when OTLP is disabled (the default). |

**Machine-readable schema (0.27.0).** Every JSON message in this document
is described by [`schemas/siphon-ai.v1.json`](../schemas/siphon-ai.v1.json)
— a JSON Schema (draft 2020-12) generated from the Rust wire types and
drift-checked in CI (including every example in this file). Point your
editor, validator, or code generator at it. Notes: messages are
discriminated by `type`; validate against `$defs/BridgeOut`
(SiphonAI→server) or `$defs/BridgeIn` (server→SiphonAI) when you know the
direction — three discriminators (`hold`, `resume`, `mark`) exist in both.

**Server SDKs (0.28.0).** Writing your server in Python or TypeScript?
The SDKs in [`sdks/`](../sdks/) implement this protocol — typed events,
paced 20 ms audio framing, close semantics — so you write handlers, not
wire code. Their test suites validate against the schema and every
example in this document, so they track the spec release-for-release.

**Conformance testkit (0.29.0).** `siphon-ai-testkit` plays the daemon's
side of this protocol against your server — scripted calls, every message
schema-validated, framing/pacing/close semantics asserted — and exits
non-zero on any violation, so "conformant with protocol v1" is a claim
your CI can check. See [`CONFORMANCE.md`](CONFORMANCE.md).
The binary audio framing is described by the schema's `x-binary-frames`
annotation.

If the upgrade fails (4xx/5xx) or the TLS handshake fails, SiphonAI
treats the call as failed and emits a CDR with `bridge_error`. There is
no automatic retry within a single call.

---

## 2. Frame types

The protocol uses **two** WebSocket frame types:

| Frame | Carries | Direction |
|---|---|---|
| **Text** | One JSON control message. UTF-8. | Both. |
| **Binary** | One audio frame. Raw PCM16, no header. | Both. |

Text and binary frames may be interleaved freely; the server MUST handle
either at any time on an active connection.

### 2.1 Text frame envelope

Every text frame is a single JSON object with at least:

```json
{ "type": "<message-type>", "call_id": "<call_id>" }
```

Fields:

- `type` — required. One of the values in §3 / §4.
- `call_id` — required. SiphonAI's internal call ID (NOT the SIP `Call-ID`,
  which appears in `start.sip.call_id`). Must match the value SiphonAI
  sent in `start`. Servers MUST echo it on every message they send;
  SiphonAI rejects messages whose `call_id` doesn't match.
- `seq` — present only on SiphonAI→server messages (see §3). Servers MUST
  NOT include `seq` in their messages.

Text frames larger than **256 KiB** are rejected by SiphonAI (and SHOULD
be rejected by the server). One frame = one message — no JSON streaming
or fragmentation.

### 2.2 Binary frame envelope

A binary frame is **exactly one audio chunk**: raw PCM16, little-endian,
mono, no header.

The chunk size is fixed for the lifetime of the connection by the
`audio` block in the `start` message:

| `sample_rate` | Samples per frame | Bytes per frame |
|---:|---:|---:|
| 8000 | 160 | 320 |
| 16000 | 320 | 640 |

Frame cadence is **50 fps** (every 20 ms). Servers SHOULD pace outbound
audio at the same cadence to avoid buffer churn; SiphonAI tolerates
bursts up to the 200 ms outbound buffer (§5.5).

A frame of the wrong **size** (not the negotiated 20 ms — 320 bytes @ 8 kHz,
640 @ 16 kHz) is **dropped** and the server is told with
`error { code: "audio_format" }`. This is **non-fatal**: the bad frame is
discarded, the call continues, and the error is **rate-limited** (the first
bad frame, then at most one per second) so a misconfigured server can't
flood the WS. A raw PCM16 frame carries no sample-rate or channel metadata,
so the daemon validates only the byte length and otherwise assumes the
negotiated rate + mono — send the wrong rate or stereo and it is
interpreted, not detected.

---

## 3. SiphonAI → Server messages

Every SiphonAI→server text frame includes `seq` — a per-call,
monotonically increasing 64-bit unsigned integer starting at 0 on
`start` and incrementing by 1 with every subsequent message SiphonAI
sends. `seq` is a debugging aid, not a flow-control mechanism. Servers
MAY use it to detect dropped frames in their own logs.

### 3.1 `start` — sent immediately on connect

```json
{
  "type": "start",
  "version": "1",
  "call_id": "siphon-7f3a9b21",
  "seq": 0,
  "from": "+13125551212",
  "to": "5000",
  "direction": "inbound",
  "audio": {
    "encoding": "pcm16le",
    "sample_rate": 8000,
    "channels": 1,
    "frame_ms": 20
  },
  "sip": {
    "call_id": "abc123@pbx.example.com",
    "headers": {
      "User-Agent": "Cisco-CP8841",
      "P-Asserted-Identity": "<sip:+13125551212@pbx.example.com>"
    }
  }
}
```

| Field | Type | Notes |
|---|---|---|
| `version` | string | Currently `"1"`. Strings, not numbers. |
| `from` | string | E.164 number or SIP user; may be empty if PBX strips it. |
| `to` | string | The dialed digits / extension / SIP user. |
| `direction` | string | `"inbound"` (SiphonAI answered the call) or `"outbound"` (SiphonAI placed it — outbound origination, 0.6.0). An outbound bot typically speaks first. Additive; the protocol version stays `"1"`. |
| `retrieved` | bool \| absent | `true` on the `start` of a session that is picking up a **previously parked** call (park/retrieve, 0.7.0; §4.9). **Absent** (not `false`) on a normal inbound/outbound `start`. A retrieve always opens a *fresh* session — `seq` restarts at 0 and there is no replay of pre-park traffic — so a server that ignores this field simply treats it as a brand-new call. |
| `reconnected` | bool \| absent | `true` on the `start` of a session that **resumes a call after an unexpected WS drop** (0.7.3, opt-in via `[bridge].ws_reconnect_enabled`; §5.7). SiphonAI re-dialed the same `ws_url` for the same `call_id`; the server should drop any handler it still holds for this call and treat this socket as the live one. Like `retrieved`, a fresh session — `seq` restarts at 0, no replay of pre-drop audio/events — and **absent** (not `false`) otherwise, so a server that ignores it treats the call as brand-new. Distinct from `retrieved` (operator picking up a *parked* call). See §5.7 for the reconnect behaviour. |
| `audio.encoding` | string | `"pcm16le"` only in v1. |
| `audio.sample_rate` | int | `8000` or `16000`. Set by the negotiated SIP codec. On a **delayed-offer** call (offerless INVITE, RFC 3264; 0.9.0) the codec isn't known until the SDP answer arrives in the ACK, so `start` — and the whole WS session — is **deferred by one SIP round-trip** versus an early-offer call. No shape change: `start` looks identical; it just arrives slightly later. |
| `audio.channels` | int | `1` only in v1. |
| `audio.frame_ms` | int | `20` only in v1. |
| `sip.call_id` | string | The SIP `Call-ID` of the call's dialog (the inbound INVITE's, or the one SiphonAI generated for an outbound call). |
| `sip.headers` | object | Selected SIP headers, by name. The set is config-driven (`bridge.forward_headers` allowlist) — never assume any specific header is present. |
| `srtp` | object \| absent | Present when the call's media leg was negotiated as SRTP; **absent** when the leg is plaintext `RTP/AVP` (the v0.1.0 / v0.2.0 default). Servers MUST treat absence and the v1 shape (no `srtp` key) as identical — this field was added in 0.3.0 and the protocol version stays `"1"`. |
| `srtp.exchange` | string | `"sdes"` (RFC 4568, master key exchanged via `a=crypto:` on the SIP signalling plane) or `"dtls"` (RFC 5764 DTLS-SRTP, key derived from a DTLS handshake over the media path). |
| `srtp.profile` | string | The negotiated SRTP crypto suite identifier, exactly as it appears on the wire — for SDES, the `a=crypto:` `crypto-suite` token; for DTLS-SRTP, the negotiated profile name. Examples: `"AES_CM_128_HMAC_SHA1_80"`, `"AES_256_CM_HMAC_SHA1_80"`, `"AEAD_AES_256_GCM"`, `"SRTP_AES128_CM_SHA1_80"`. String rather than enum because new suites land at the IANA registry independent of this release. |
| `verstat` | object \| absent | STIR/SHAKEN verification verdict (RFC 8224/8225). **Absent** unless `[security.stir_shaken].enabled`. Added in 0.4.0; the protocol version stays `"1"`, so a server that doesn't know the field ignores it. |
| `verstat.attest` | string \| absent | Claimed SHAKEN attestation: `"A"` (full) / `"B"` (partial) / `"C"` (gateway). Absent when no valid attestation was present. **Trust it only when the booleans below all hold** — a present `attest` with `signature_valid: false` is an unverified claim. |
| `verstat.orig_tn` | string \| absent | Originating TN from the PASSporT `orig` claim. |
| `verstat.orig_passed` | bool | `orig` TN matched the SIP `From`. |
| `verstat.dest_passed` | bool | A `dest` TN matched the SIP `To` / request URI. |
| `verstat.cert_chain_valid` | bool | Signing cert chained to a configured STI-PA trust anchor. |
| `verstat.signature_valid` | bool | ES256 signature over the PASSporT verified against that cert. |
| `verstat.iat_passed` | bool | PASSporT `iat` was within the configured freshness window (replay protection). Added in 0.4.1; `false` for a stale/future/missing `iat`. Like the other booleans, it's part of the composite a server should treat as "trusted". |
| `verstat.error` | string \| absent | Human-readable reason when verification did not fully pass; absent on success. |
| `trace_context` | object \| absent | W3C trace context for this call's daemon-side OTLP trace (0.23.0). **Present only when `[observability.otlp]` is enabled**; absent otherwise (the default), so the protocol version stays `"1"` and a server that doesn't know the field ignores it. The same values are sent as `traceparent`/`tracestate` headers on the upgrade request — read whichever is easier for your stack. A server that adopts this as the parent of its own spans appears in the same trace waterfall as the daemon's SIP/media spans. |
| `trace_context.traceparent` | string | W3C `traceparent`: `00-<32 hex trace-id>-<16 hex span-id>-<2 hex flags>`. The trace-id identifies the whole call's trace; the span-id is the daemon's call-root span. The flags byte reflects the daemon's sampling decision (`01` sampled, `00` not) — honouring it keeps sampling consistent across both services. |
| `trace_context.tracestate` | string \| absent | W3C `tracestate` (vendor key/value list). Absent when there is nothing to forward — which is the common case. |

The `srtp` field is omitted from the JSON when SRTP is off; a v1
WS server that doesn't know about it sees exactly the v0.2.0 shape.

**DTLS-SRTP (0.3.0 W2): produced.** When an inbound `UDP/TLS/RTP/SAVPF`
offer is accepted under `srtp = "preferred"` or `"required"`, the
field is populated with `{ exchange: "dtls", profile:
"AES_CM_128_HMAC_SHA1_80" }`. The profile is best-guess pre-handshake
— RFC 5764 mandates that suite as the baseline; the actual
negotiated profile may be stronger (AEAD-GCM) when both sides
support it, but `start` fires before the DTLS handshake completes
so we can't carry the post-handshake choice. Servers that need the
true negotiated profile should wait for a quality assessment event
rather than trusting `start.srtp.profile` exactly.

**SDES (`exchange: "sdes"`): produced.** Inbound, when an offer carries
`a=crypto:` under a non-`off` `[media].srtp`, SiphonAI answers `RTP/SAVP`
and `start.srtp` reports `{ exchange: "sdes", profile: <suite> }`.
**Outbound (0.7.x):** a call placed through a gateway with
`[[gateway]].srtp = "preferred" | "required"` *offers* SDES — SiphonAI mints
the master key, sends `RTP/SAVP` + `a=crypto:`, and on a 2xx that accepts it
populates `start.srtp` the same way (`required` fails the call if the trunk
answers plaintext; `preferred` continues unencrypted with `start.srtp`
absent). So `start.srtp` is now populated on both inbound and outbound calls.

See [`docs/CONFIG.md`](CONFIG.md) `[media].srtp` (inbound) and
`[[gateway]].srtp` (outbound) for the operator-facing switches, and
[`docs/OUTBOUND.md`](OUTBOUND.md) for the outbound SRTP guide.

**`verstat` (0.4.0): produced when verification is enabled.** With
`[security.stir_shaken].enabled = true`, the accept path verifies each
inbound INVITE's `Identity` header and populates this field on `start`
(an INVITE with no `Identity` header yields an unsigned verdict —
`signature_valid: false`, `attest` absent — rather than omitting the
field). When verification is disabled the field is absent entirely. A
server that wants to apply its own fraud policy reads the booleans
(treating a present-but-failed verdict as untrusted), not `attest` alone.
The daemon's own `min_attestation` gate is a separate operator switch —
see [`docs/CONFIG.md`](CONFIG.md) `[security]` for it and the
attestation-gate policy matrix.

A server MUST begin sending audio (or send a `hangup`) within the
**start-deadline** (default 5 s, `[bridge].server_start_deadline_secs`)
of receiving `start`, otherwise SiphonAI emits
`error { code: "server_too_slow" }`, follows it with `stop`, and tears
down. Only the *first audio frame* (or a `hangup`) satisfies the
deadline — a control message alone (e.g. `mark`) does not. Operators
whose servers legitimately need longer to first audio (cold-start
LLM/TTS) can raise the value; `0` disables the deadline.

### 3.2 `speech_started` / `speech_stopped` — VAD events

Emitted whenever SiphonAI's voice-activity detector (forge-vad) sees the
caller **start** and **stop** speaking. They are **always emitted** — there
is no enable flag; a server that doesn't need them just ignores them. (These
are the same VAD signals that drive barge-in — see `[bridge.barge_in]` in
`docs/CONFIG.md`.)

```json
{ "type": "speech_started", "call_id": "...", "seq": 42, "ts_ms": 1234 }
{ "type": "speech_stopped", "call_id": "...", "seq": 67, "ts_ms": 1890, "duration_ms": 656 }
```

`ts_ms` is monotonic milliseconds since `start` was sent (NOT wall-clock);
`speech_stopped` also carries `duration_ms` (the length of the speech run).

The barge-in **mode** doesn't change *whether* these are sent, only what
SiphonAI does alongside a `speech_started`: `auto_clear` (the default) also
flushes pending outbound playout; `notify_only` leaves that to the server.
One nuance: with `auto_clear` **and** a configured
`[bridge.barge_in].debounce_ms`, a `speech_started` that the debounce gate
classifies as the bot's own echo/noise (a brief start→stop while the bot is
playing) is suppressed together with its `speech_stopped`, so the server
never sees that provisional pair.

### 3.3 `hold` / `resume` — peer paused or resumed media

Emitted when a mid-dialog re-INVITE flips the audio direction
across the `sendrecv` boundary. SiphonAI mirrors the peer's
direction per RFC 3264 §6.1 and reports the transition here so the
server can stop / resume sending audio. Servers SHOULD pause
outbound audio for the duration of the hold — the peer isn't
listening — and resume on `resume`.

```json
{ "type": "hold", "call_id": "...", "seq": 95, "direction": "sendonly" }
{ "type": "resume", "call_id": "...", "seq": 142 }
```

`direction` is one of `"sendonly"`, `"recvonly"`, or `"inactive"`.
Transitions between non-`sendrecv` states (e.g. `sendonly` →
`inactive`) do NOT emit a second `hold` — the server already knows
the call is paused. The matching `resume` arrives when the peer
returns to `sendrecv`.

### 3.4 `dtmf` — caller pressed a key

```json
{ "type": "dtmf", "call_id": "...", "seq": 88, "digit": "5", "duration_ms": 120, "method": "rfc2833" }
```

`digit` is one of `0-9 * # A B C D`.
`method` is `"rfc2833"` or `"inband"` — depending on detection source.

### 3.5 `mark` — playback marker fired

The acknowledgement to a server-initiated `mark` (§4.2). SiphonAI emits
this when the audio queued *before* the server's `mark` request has
been fully played out into the call.

```json
{ "type": "mark", "call_id": "...", "seq": 91, "name": "greeting_done" }
```

### 3.6 `silence_detected` — caller has been silent

```json
{ "type": "silence_detected", "call_id": "...", "seq": 102, "duration_ms": 3000 }
```

Fired when the caller has produced no VAD speech for at least
`[bridge].silence_threshold_ms` (default 3 s; configurable, per-route
override; `0` disables the event). The `duration_ms` reports actual
elapsed time at fire, which may exceed the threshold by up to one
poll cadence (500 ms). The event fires **once per silence stretch** —
the next `silence_detected` only after a speech → silence cycle.

Typical use: prompt the caller ("are you still there?") or escalate
to a human after a configurable wait.

### 3.7 `dead_air_detected` — no audio in either direction

```json
{ "type": "dead_air_detected", "call_id": "...", "seq": 103, "duration_ms": 10000 }
```

Fired when **neither** caller VAD speech **nor** outbound playout from
the WS server has been observed for at least
`[bridge].dead_air_threshold_ms` (default 10 s; configurable,
per-route override; `0` disables). Re-fires every time the threshold
elapses without either side producing audio — a still-dead call
generates a steady drumbeat of these events.

Distinct from `silence_detected`, which is one-sided (caller silent
but the AI may still be talking). `dead_air_detected` suggests a
hung call or connectivity issue; typical reaction is to hang up.

### 3.8 `rtp_stats` — periodic RTP/RTCP snapshot

```json
{ "type": "rtp_stats", "call_id": "...", "seq": 50, "jitter_ms": 12.5, "packet_loss_ratio": 0.004 }
```

Fired every `[bridge].rtp_stats_interval_ms` (default 5 s; configurable,
per-route override; `0` disables). The cadence mirrors RTCP's compound-
report interval (RFC 3550 §6.2) so values track the underlying RTCP
arrivals.

Fields are JSON `null` (omitted) until forge has reported its first
quality assessment for the call:

| Field                | Type            | Notes |
|----------------------|-----------------|-------|
| `jitter_ms`          | float \| null   | Estimated inter-arrival jitter. `null` if no RTCP RR has arrived yet. After a `QualityRestored` event in forge, this resets to `0.0` — distinct from `null`. |
| `packet_loss_ratio`  | float \| null   | Loss as a ratio in `[0.0, 1.0]` (NOT a percent). Same `null` / `0.0` distinction. |
| `rtcp_rtt_ms`        | float \| null   | Mean round-trip time over the reporting window. `null` until forge originates its own RTCP SRs (deferred to 0.3.1). Distinct from `0.0`, which is degenerate; once populated, the field is sticky — a window with no fresh RTT sample preserves the last measurement rather than reverting to `null`. |

Codec and sample-rate are constant for a call — consumers should
correlate to the `start` message (§3.1) rather than expecting them
on every snapshot.

### 3.9 `stop` — call ended

```json
{ "type": "stop", "call_id": "...", "seq": 200, "reason": "caller_hangup" }
```

`reason` is one of:

| Value | Meaning |
|---|---|
| `caller_hangup` | The far-end SIP party sent BYE. |
| `server_hangup` | The WS server sent `hangup` (§4.3). |
| `transfer` | A blind transfer (REFER) was accepted; SiphonAI is releasing the leg. |
| `park` | The call was **parked** (0.7.0; §4.9). The WS session is being detached, **not** the call torn down — the SIP dialog and RTP stay up and the caller hears hold music. The server learns "you were parked, not hung up"; the call may later be retrieved onto a fresh session (with `start.retrieved: true`). |
| `ws_disconnect` | The WS connection closed unexpectedly mid-call. SiphonAI plays the configured fallback prompt and tears down the SIP leg. |
| `error` | A fatal error occurred; an `error` message preceded this. |

`stop` is the last message SiphonAI sends on the connection. SiphonAI
then closes the WebSocket cleanly (close code 1000).

### 3.10 `error` — fatal error

```json
{ "type": "error", "call_id": "...", "seq": 201, "code": "rtp_timeout", "message": "no RTP for 30s on leg A" }
```

`code` is one of:

| Code | Meaning |
|---|---|
| `rtp_timeout` | No incoming RTP for the configured idle period (`media.rtp_idle_timeout_ms`). |
| `audio_format` | Server sent an audio frame of an unexpected **size** — not the negotiated 20 ms frame (320 bytes @ 8 kHz, 640 @ 16 kHz). Only the byte length is checkable: a binary audio frame carries no sample-rate or channel metadata, so the daemon assumes the negotiated rate + mono. **Non-fatal:** the frame is dropped, the call continues, and the error is rate-limited (§2.2) — no `stop` follows. |
| `protocol_error` | A WS message was malformed JSON, used an unknown `type`, or had a `call_id` that doesn't match the connection. **Fatal** and definitive — the call is torn down and (with `ws_reconnect_enabled`) **not** reconnected, since a buggy server would just repeat it. |
| `server_too_slow` | Server didn't send its first audio frame (or a `hangup`) within the start-deadline of `start` — `[bridge].server_start_deadline_secs`, default 5 s (§3.1). |
| `transfer_failed` | A REFER was attempted but the far end rejected it. |
| `conference_failed` | A `conference_join` (§4.8) was refused: conferencing disabled, room or per-room cap reached, sample-rate mismatch, or already joined. The call continues on its direct pair. |
| `park_failed` | A `park` (§4.9) was refused: park disabled (`[park].enabled = false`) or `[park].max_parked` reached. The call continues unparked on its current WS session — no `stop` follows this `error`. |
| `hold_failed` | A `hold` / `resume` (§4.10) re-INVITE was rejected by the peer, timed out, or lost glare resolution. The call stays in its **prior** media state (a failed hold never drops it) — no `stop` follows this `error`. |
| `internal` | SiphonAI internal error. The `message` field has details — e.g. `ws keepalive timeout` when a half-open connection fails the §5.6 keepalive (best-effort: an unresponsive peer may never receive it). |

A **fatal** `error` is always followed by `stop` (with `reason:
"error"`) and a clean close. The **non-fatal** codes —
`conference_failed`, `park_failed`, `hold_failed`, and `audio_format` — are
the exception: they report a rejected control request or a discarded frame,
the call continues, and no `stop` follows.

> **Codec negotiation failure is not a WS error.** There is no
> `codec_unsupported` code: codec selection happens entirely at the SIP
> layer, and an INVITE whose SDP offers no codec SiphonAI supports is
> rejected with **`488 Not Acceptable Here`** — *before* any WS bridge is
> opened. The WS connection only exists once a codec is agreed, so a
> codec mismatch can never surface here.

### 3.11 `recording_started` / `recording_stopped` / `recording_failed` — recording lifecycle (0.5.0)

Emitted when call recording is on (`[recording].mode`). `recording_started`
fires automatically on `always`, or in response to a `start_recording`
control on `on_demand`; `recording_stopped` on call end or `stop_recording`.

```json
{ "type": "recording_started", "call_id": "...", "seq": 12, "recording_id": "..." }
{ "type": "recording_stopped", "call_id": "...", "seq": 40, "recording_id": "..." }
{ "type": "recording_failed",  "call_id": "...", "seq": 13, "recording_id": "...", "reason": "disk full" }
```

`recording_id` identifies the recording (one per call in this release).
Recording is best-effort — `recording_failed` never tears the call down.

### 3.12 `conference_joined` / `conference_left` / `participant_joined` / `participant_left` — conference room events (0.7.0)

Emitted for a call that has joined a conference room (§4.8). Additive —
the protocol version stays `"1"`; a server that never sends
`conference_join` never sees these.

```json
{ "type": "conference_joined",  "call_id": "...", "seq": 14, "room_id": "support-7", "participants": 2 }
{ "type": "conference_left",    "call_id": "...", "seq": 40, "room_id": "support-7", "reason": "left" }
{ "type": "participant_joined", "call_id": "...", "seq": 15, "room_id": "support-7", "participant_call_id": "siphon-b" }
{ "type": "participant_left",   "call_id": "...", "seq": 38, "room_id": "support-7", "participant_call_id": "siphon-b" }
```

- **`conference_joined`** — the response to *this* session's
  `conference_join`. `participants` is the member-call count at the
  moment of joining (this call included); it's a snapshot — live
  changes arrive as the events below.
- **`conference_left`** — this call left the room. `reason` is `"left"`
  (the server sent `conference_leave`) or `"room_closed"` (the room
  ended underneath it — e.g. an operator force-ended it). Either way
  the direct caller↔WS audio pair is restored and the call continues.
- **`participant_joined` / `participant_left`** — fan-out: ANOTHER call
  joined or left the room this call is in. `participant_call_id` is
  that other call's bridge `call_id`, distinct from the envelope
  `call_id`, which is always *your* call. These let a bot track who
  else is in the room; cross-call control (adding/removing others) is
  the admin API's job, not the WS surface (a session only acts on its
  own call).

While joined, every other event (`dtmf`, `speech_*`, `mark`, …) keeps
flowing for this call's own leg. Audio frames are unchanged on the
wire: SiphonAI sends the room mix (minus this call's own contribution)
as the inbound binary frames, and the server's outbound audio is mixed
into the room.

### 3.13 `held` / `resumed` — your hold/resume request took effect (0.7.2)

Confirmations that a server-initiated `hold` / `resume` (§4.10) completed —
the re-INVITE was acknowledged by the peer. Sent **after** the SIP
round-trip, so the server knows the hold is real before relying on it.

```json
{ "type": "held",    "call_id": "...", "seq": 61 }
{ "type": "resumed", "call_id": "...", "seq": 80 }
```

> **`held`/`resumed` (your request) vs. `hold`/`resume` (§3.3, the peer).**
> These are **different messages**. `held`/`resumed` confirm *your* `hold`/
> `resume` succeeded. The §3.3 `hold`/`resume` events fire when the **far
> end** put *you* on hold (an incoming re-INVITE) — unrelated to anything
> you sent. A failed hold/resume request comes back as
> `error { code: "hold_failed" }` (§3.10) instead, and the call is
> unchanged.

---

## 4. Server → SiphonAI messages

Server messages MUST include `type` and `call_id`. They MUST NOT include
`seq`. Unknown `type` values trigger `error { code: "protocol_error" }`.

### 4.1 `clear` — drop pending outbound playback (barge-in)

```json
{ "type": "clear", "call_id": "..." }
```

Discards any audio queued for playout into the call but not yet sent.
Audio that has already left the network has, of course, already been
played to the caller and cannot be unsent. Pending `mark` events that
were queued behind the cleared audio are dropped without firing.

### 4.2 `mark` — insert a playback marker

```json
{ "type": "mark", "call_id": "...", "name": "greeting_done" }
```

Inserts a marker at the current tail of the outbound queue. When the
marker reaches the head (i.e. all audio queued before it has been sent),
SiphonAI emits a SiphonAI→server `mark` (§3.4) with the same `name`.

`name` is opaque to SiphonAI: ASCII, ≤64 chars, server-chosen.

### 4.3 `hangup` — terminate the call

```json
{ "type": "hangup", "call_id": "...", "cause": "normal" }
```

`cause` is optional; default `"normal"`. Defined values:

| Cause | SIP response |
|---|---|
| `normal` | BYE on an established dialog, or 487 on an early dialog. |
| `rejected` | 603 Decline (the call hasn't been answered). |
| `busy` | 486 Busy Here. |
| `not_acceptable` | 488 Not Acceptable Here. |

After a successful hangup, SiphonAI sends `stop` with
`reason: "server_hangup"` and closes the connection.

### 4.4 `transfer` — call transfer (REFER), blind or attended

```json
{ "type": "transfer", "call_id": "...", "target": "sip:agent@example.com" }
```

`target` MUST be a SIP or SIPS URI. SiphonAI sends a blind REFER with
that target. On a 2xx final response, SiphonAI sends BYE on the same
dialog (the "REFER + BYE" pattern from RFC 5589 §6.1), then emits
`stop` with `reason: "transfer"` and closes the WebSocket. NOTIFYs
that the PBX sends after the REFER are accepted but not surfaced over
the WS in v1. On a non-2xx response (or a local failure — bad target
URI, dialog gone), SiphonAI emits `error { code: "transfer_failed" }`
and the call continues.

**Attended transfer** (0.6.1, additive — version stays `"1"`): add
`replaces_call_id` naming an **answered outbound call** (a consult leg
placed via `POST /admin/v1/calls`, identified by the `call_id` that
endpoint returned):

```json
{ "type": "transfer", "call_id": "...", "replaces_call_id": "siphon-..." }
```

SiphonAI sends a REFER whose `Refer-To` carries a `Replaces` parameter
built from the consult call's dialog, so the transferee connects
directly to the consulted party (RFC 5589 §7). With `replaces_call_id`
set, `target` is **optional** — the default is the consult dialog's
remote target (the Contact from its 200 OK), which is normally
correct; send `target` explicitly only to override the reachable URI
(e.g. through an SBC). The same 2xx / non-2xx semantics as blind
transfer apply. The consult leg is not torn down by SiphonAI at REFER
time — the transferee's INVITE-with-Replaces takes it over, and the
consult call ends through its normal teardown. If `replaces_call_id`
doesn't name a currently-answered outbound call (unknown, not yet
answered, or already ended), SiphonAI emits
`error { code: "transfer_failed" }` and the call continues.

To **cancel** a consultation, simply send `hangup` on the consult
call's own WS session (or use the admin force-hangup) — no dedicated
message exists, and the original call is unaffected.

### 4.5 `send_dtmf` — emit DTMF toward the caller

```json
{ "type": "send_dtmf", "call_id": "...", "digit": "1", "duration_ms": 200 }
```

`digit` is one of `0-9 * # A B C D`. SiphonAI generates an RFC 2833
event of `duration_ms` (clamped to `[40, 2000]`).

### 4.6 `mute` / `unmute` — sustained AI-side mute

```json
{ "type": "mute",   "call_id": "..." }
{ "type": "unmute", "call_id": "..." }
```

`mute` suspends AI-side playout to the caller until `unmute` arrives.
On receipt of `mute`, SiphonAI:

1. Sets a per-call gate that drops audio bytes the WS server continues
   to stream (the channel is still drained — the WS server is *not*
   back-pressured — so other control messages keep flowing).
2. Flushes audio already queued into the media engine so the caller
   hears silence immediately, not after the queued tail plays out.

`unmute` clears the gate; subsequent audio frames flow into the call.
`unmute` while not muted is a no-op.

**Distinct from `clear`.** `clear` is a one-shot flush — typically
fired in response to caller barge-in — that drains pending playout
once and then accepts new audio. `mute` is sustained, requires an
explicit `unmute` to release, and is the right primitive for "put
this call on hold from the AI side."

### 4.7 `start_recording` / `stop_recording` / `pause_recording` / `resume_recording` — on-demand recording (0.5.0)

```json
{ "type": "start_recording",  "call_id": "..." }
{ "type": "stop_recording",   "call_id": "..." }
{ "type": "pause_recording",  "call_id": "..." }
{ "type": "resume_recording", "call_id": "..." }
```

Drive recording when `[recording].mode = "on_demand"`. `start_recording`
begins it (SiphonAI replies `recording_started`, or `recording_failed`);
`stop_recording` finalizes it (`recording_stopped`). `pause_recording`
**omits** the paused span from the file — the paused audio is dropped, not
silenced (e.g. while the caller reads a card number) — and `resume_recording`
continues. All are no-ops if recording isn't enabled for the call or the
control is invalid for the current state. (With `mode = "always"` recording
covers the whole call; these controls aren't needed.)

**`set_recording_consent`** (0.26.0) — record the fact that your server
captured recording consent:

```json
{ "type": "set_recording_consent", "call_id": "...", "note": "dtmf-1" }
```

When your server obtains consent itself (a DTMF "press 1 to consent", a
verbal yes your bot recognized), send this so the fact lands on the call's
CDR as `consent.server` for the audit trail. `note` is a short free-form
description (optional, truncated to 256 bytes; defaults to
`"unspecified"`). This is a **stamp, not a gate** — to gate capture on
consent, use `mode = "on_demand"` and send `start_recording` after consent.
No reply message; additive, so the protocol stays v1.

### 4.8 `conference_join` / `conference_leave` — conference rooms (0.7.0)

```json
{ "type": "conference_join",  "call_id": "...", "room_id": "support-7" }
{ "type": "conference_leave", "call_id": "..." }
```

Join this call into a named conference room, or leave the one it's in.
Requires `[conference].enabled = true` on the daemon.

- **`conference_join`** creates the room if it doesn't exist yet
  (subject to `[conference].max_rooms` and
  `max_participants_per_room`). On success SiphonAI replies
  `conference_joined` (§3.12) and the call's audio is mixed into the
  room: the bot hears the room **minus its own playout** and speaks
  into it. On failure — conferencing off, a cap reached, a sample-rate
  mismatch (a room locks to its first joiner's rate; no resampling in
  0.7.0), or already in this room — SiphonAI replies
  `error { code: "conference_failed" }` and the call continues
  unchanged on its direct pair. Joining a second room moves the call
  (it leaves the first).
- **`conference_leave`** removes the call from its room and restores
  the direct caller↔WS pair; SiphonAI replies
  `conference_left { reason: "left" }`. A no-op (no reply) if the call
  isn't in a room.

**Self-scoped (§5.3):** these act only on the session's *own* call. A
bot can put itself in or out of a room, but it cannot add or remove
*another* participant — that's the admin API's job (operator control
plane). A bot tracks the rest of the room via the `participant_joined`
/ `participant_left` fan-out events.

The room model: N calls share one mixed room, **every call keeps its
own WS session** (there is no single "host" bot). For N member calls
the room mixes 2N streams — each call's SIP caller and its bot — and
hands each side the mix minus its own input. So a caller never hears
themselves, a bot never hears its own playout, but a caller hears
their own bot and a bot hears its own caller (STT keeps working).

### 4.9 `park` — park this call (0.7.0)

```json
{ "type": "park", "call_id": "...", "slot": "lot-3" }
```

Park this call: detach the WS session and shelve the call playing hold
music, **without** ending it. Requires `[park].enabled = true` on the
daemon.

- `slot` is an **optional** human label for the hold lot (e.g. a parking
  orbit or agent name); it surfaces in `GET /admin/v1/parked` and the
  `call_parked` webhook. Omit it for an unlabeled park.
- On success SiphonAI sends `stop { reason: "park" }` (§3.9) and closes
  **this** WebSocket cleanly. The SIP dialog and RTP stay up and the
  caller hears hold music (`[park].moh_file`, or comfort noise). The
  call is **not** torn down.
- On failure — park disabled, or `[park].max_parked` reached — SiphonAI
  replies `error { code: "park_failed" }` (§3.10) and the call continues
  unparked on this session. No `stop` follows.

**Self-scoped (§5.3):** `park` acts only on the session's *own* call.

**Retrieve is operator-only.** There is no WS-server message to retrieve
a parked call — retrieval is driven by the admin API
(`POST /admin/v1/calls/:id/retrieve`, see [`docs/DEPLOY.md`](DEPLOY.md)),
which opens a **fresh** WS session with `start.retrieved: true` (§3.1).
A parked call with no WS session cannot retrieve itself; that's the
operator control plane's job, the mirror of why participants are
removed from conferences by the admin API rather than by a peer.

### 4.10 `hold` / `resume` — put the caller on hold (0.7.2)

```json
{ "type": "hold",   "call_id": "..." }
{ "type": "resume", "call_id": "..." }
```

Put **this** call's caller on hold, or bring them back. Unlike park, the
WS session **stays open** and the server resumes the call itself — this is
the primitive for "user asks to hold → bot holds → bot resumes."

- **`hold`** re-INVITEs the caller so their media goes on hold
  (`a=sendonly`/`recvonly`, RFC 3264): they hear hold music
  (`[media].moh_file`, or comfort noise) and stop sending, and SiphonAI
  stops forwarding their audio to the server (so no barge-in fires during
  hold). On success SiphonAI replies `held` (§3.13) once the re-INVITE is
  acknowledged. No-op if already held.
- **`resume`** re-INVITEs back to two-way audio and restores the direct
  caller↔server pair; SiphonAI replies `resumed`. No-op if not held.
- On failure (the peer rejects the re-INVITE, it times out, glare can't
  be resolved, or the **far end already has you on hold** — bot-hold does
  not stack on a peer-hold in this release) SiphonAI replies
  `error { code: "hold_failed" }` (§3.10) and the call stays in its
  **prior** state — a failed hold never drops it.

**Self-scoped (§5.3):** `hold` acts only on the session's *own* call.

**Distinct from `mute` (§4.6) and the `hold` *event* (§3.3).** `mute` only
silences the server's *own* audio (the caller's mic still reaches the
server, the dialog stays `sendrecv`); `hold` signals the far end and plays
hold music. The §3.3 `hold` *event* is the opposite direction — the peer
holding *you*.

---

## 5. Protocol rules

### 5.1 Ordering

- `start` is always the first message SiphonAI sends.
- `stop` is always the last.
- Between them, control messages and binary audio frames may interleave
  in any order.

### 5.2 `seq`

Per §3: monotonic, per-call, on SiphonAI→server messages only. Never
resets within a call. Wraps theoretically at 2⁶⁴; in practice the call
ends first.

### 5.3 `call_id`

The SiphonAI-internal ID. NOT the SIP `Call-ID` (which is in
`start.sip.call_id`). Server messages MUST echo it; mismatches trigger
`error { code: "protocol_error" }`.

### 5.4 Versioning

- `start.version` is currently `"1"`.
- A server unwilling to speak v1 SHOULD `hangup` immediately (with any
  `cause`) or close the WS with code 1003 ("unsupported data").
- New optional fields and new enum variants for `error.code`,
  `stop.reason`, and `hangup.cause` are additive and do NOT bump
  `version`. Servers MUST treat unknown enum values defensively (log,
  ignore, or fail soft).
- Removing a field, changing a field's type, or changing the meaning
  of an existing enum variant DOES bump `version` to `"2"`.

### 5.5 Audio backpressure

SiphonAI buffers up to **200 ms** of outbound audio (10 frames at
50 fps). Beyond that, the **oldest** frames are dropped and a
`siphon_ai_outbound_audio_frames_dropped_total` metric is incremented.
This prevents a slow caller-side from causing unbounded growth when the
server bursts audio.

A server that needs precise timing should use `mark` to know when its
audio has actually been played, rather than counting bytes sent.

### 5.6 Liveness

WebSocket keepalive is enabled by default. SiphonAI sends a ping every
`[bridge].ws_ping_interval_secs` (default 15 s); if a server fails to
pong within `[bridge].ws_pong_timeout_secs` (default 10 s) of an
outstanding ping, SiphonAI treats the connection as half-open. It makes
a best-effort emit of
`error { code: "internal", message: "ws keepalive timeout" }` (the peer
is, by definition, unresponsive — so this may not arrive) and drops the
session. With `[bridge].ws_reconnect_enabled` a keepalive timeout is an
**unexpected drop** that enters the reconnect path (§5.7); otherwise it
tears the call down. Setting either interval to `0` disables keepalive.
Servers MAY ping SiphonAI; SiphonAI always pongs.

### 5.7 WS disconnect mid-call

**Default (`[bridge].ws_reconnect_enabled = false`).** If the WS
connection closes (cleanly or otherwise) before SiphonAI has sent
`stop`, SiphonAI:

1. Stops sending audio frames over the (now-closed) WS.
2. Plays the configured `bridge.fallback_prompt_path` audio file (or
   silence) into the call.
3. Sends SIP BYE.
4. Emits a CDR with `stop_reason = "ws_disconnect"`.

**Opt-in reconnect (`[bridge].ws_reconnect_enabled = true`, 0.7.3).** An
**unexpected** drop instead triggers automatic reconnect: SiphonAI keeps
the SIP call up on hold music (`[media].moh_file`, or comfort silence),
re-dials the **same** `ws_url`, and resumes the call on a fresh session —
a new `start` carrying `reconnected: true` (§3.1), with `seq` restarting
at 0 and **no replay** of pre-drop audio or events. The server should
drop any handler it still holds for that `call_id` and treat the new
socket as the live one. If no redial succeeds within
`[bridge].ws_reconnect_max_secs` (default 30 s), SiphonAI falls back to
the teardown above (`ws_disconnect`).

> **Ending a call with reconnect on:** send the `hangup` control message
> (§4) — that's an explicit end and is never reconnected. A bare WS
> socket close (even a clean `1000`) is treated as an unexpected drop and
> reconnected. With reconnect **off**, the v1 behaviour is unchanged: any
> close ends the call.

What counts as "unexpected": a server-side close before `stop`/`hangup`,
a connection/TLS error, or a keepalive timeout (§5.6). A `stop` SiphonAI
already sent, or a `hangup` from the server, is the call ending — never
reconnected.

### 5.8 No fragmentation

SiphonAI does not produce nor accept fragmented WebSocket messages
(continuation frames). Each text or binary message is a single complete
WS frame.

---

## 6. Reserved / future

The following are intentionally reserved and MUST NOT be used by v1
servers:

- `type` values starting with `_` (reserved for SiphonAI experimental
  messages).
- Field names starting with `x-` (reserved for vendor extensions).
- WebSocket close codes in the 4000-4099 range (reserved for SiphonAI
  application close codes; see §5.4).

---

## 7. Examples

### 7.1 Echo server skeleton

```
S→C: start { call_id, audio: { sample_rate: 8000, ... } }
C→S: <binary audio frames @ 50 fps>
S→C: <binary audio frames @ 50 fps, echoed back>
S→C: stop { reason: "caller_hangup" }
```

### 7.2 Greeting + listen

```
C→S: start
S→C: <binary frames: "Hello, how can I help you?">
S→C: mark { name: "greeting_done" }
... 200 ms later, audio drained ...
C→S: mark { name: "greeting_done" }
   ↑ Server now knows the greeting has finished playing and can
     enable barge-in detection from this point.
C→S: dtmf { digit: "1" }   ← caller pressed 1
S→C: hangup { cause: "normal" }
C→S: stop { reason: "server_hangup" }
```

### 7.3 Barge-in with `clear`

```
S→C: <binary frames: "Here are our hours of operation, Monday...">
C→S: speech_started   ← caller started talking
S→C: clear            ← server immediately stops the prompt
C→S: <binary frames: silence>  ← prompt audio that was already in flight
                                  finishes playing, queue is empty
... server runs STT on caller speech ...
S→C: <binary frames: response audio>
```

---

## 8. See also

- `docs/CONFIG.md` — `bridge.*`, `routes.*.bridge.*` config reference.
- `docs/DIALPLAN.md` — how a route picks a `ws_url` and bridge config.
- `crates/bridge/src/protocol.rs` — Rust types matching this spec.
- `examples/echo-ws-server-python/` — reference echo server.
