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
| Headers | Servers may inspect any HTTP header on the upgrade request. SiphonAI sets `User-Agent: siphon-ai/<version>` and forwards a `X-Siphon-Call-Id: <call_id>` header for log correlation. |

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

Frames of the wrong size, the wrong sample rate, or non-mono content are
rejected with `error { code: "audio_format" }` and the call is torn down.

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
| `direction` | string | `"inbound"` only in v1. SiphonAI never originates. |
| `audio.encoding` | string | `"pcm16le"` only in v1. |
| `audio.sample_rate` | int | `8000` or `16000`. Set by the negotiated SIP codec. |
| `audio.channels` | int | `1` only in v1. |
| `audio.frame_ms` | int | `20` only in v1. |
| `sip.call_id` | string | The SIP `Call-ID` from the inbound INVITE. |
| `sip.headers` | object | Selected SIP headers, by name. The set is config-driven (`bridge.forward_headers` allowlist) — never assume any specific header is present. |

A server MUST begin sending audio (or send a `hangup`) within 5 seconds
of receiving `start`, otherwise SiphonAI emits
`error { code: "server_too_slow" }` and tears down.

### 3.2 `speech_started` / `speech_stopped` — VAD events (optional)

Emitted only when `bridge.vad = true` is configured. Default off.

```json
{ "type": "speech_started", "call_id": "...", "seq": 42, "ts_ms": 1234 }
{ "type": "speech_stopped", "call_id": "...", "seq": 67, "ts_ms": 1890, "duration_ms": 656 }
```

`ts_ms` is monotonic milliseconds since `start` was sent (NOT wall-clock).

### 3.3 `dtmf` — caller pressed a key

```json
{ "type": "dtmf", "call_id": "...", "seq": 88, "digit": "5", "duration_ms": 120, "method": "rfc2833" }
```

`digit` is one of `0-9 * # A B C D`.
`method` is `"rfc2833"` or `"inband"` — depending on detection source.

### 3.4 `mark` — playback marker fired

The acknowledgement to a server-initiated `mark` (§4.2). SiphonAI emits
this when the audio queued *before* the server's `mark` request has
been fully played out into the call.

```json
{ "type": "mark", "call_id": "...", "seq": 91, "name": "greeting_done" }
```

### 3.5 `stop` — call ended

```json
{ "type": "stop", "call_id": "...", "seq": 200, "reason": "caller_hangup" }
```

`reason` is one of:

| Value | Meaning |
|---|---|
| `caller_hangup` | The far-end SIP party sent BYE. |
| `server_hangup` | The WS server sent `hangup` (§4.3). |
| `transfer` | A blind transfer (REFER) was accepted; SiphonAI is releasing the leg. |
| `ws_disconnect` | The WS connection closed unexpectedly mid-call. SiphonAI plays the configured fallback prompt and tears down the SIP leg. |
| `error` | A fatal error occurred; an `error` message preceded this. |

`stop` is the last message SiphonAI sends on the connection. SiphonAI
then closes the WebSocket cleanly (close code 1000).

### 3.6 `error` — fatal error

```json
{ "type": "error", "call_id": "...", "seq": 201, "code": "rtp_timeout", "message": "no RTP for 30s on leg A" }
```

`code` is one of:

| Code | Meaning |
|---|---|
| `rtp_timeout` | No incoming RTP for the configured idle period (`media.rtp_idle_timeout_ms`). |
| `codec_unsupported` | SDP offered no codec SiphonAI supports. |
| `audio_format` | Server sent audio with an unexpected size, sample rate, or layout. |
| `protocol_error` | A WS message was malformed JSON, used an unknown `type`, or had a `call_id` that doesn't match the connection. |
| `server_too_slow` | Server didn't begin sending audio within 5 s of `start`. |
| `transfer_failed` | A REFER was attempted but the far end rejected it. |
| `internal` | SiphonAI internal error. The `message` field has details. |

`error` is always followed by `stop` (with `reason: "error"`) and a
clean close.

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

### 4.4 `transfer` — blind transfer (REFER)

```json
{ "type": "transfer", "call_id": "...", "target": "sip:agent@example.com" }
```

`target` MUST be a SIP or SIPS URI. SiphonAI sends a REFER with that
target. On a 202-Accepted that proceeds to NOTIFY 200, SiphonAI sends
`stop` with `reason: "transfer"` and closes. On rejection, SiphonAI
sends `error { code: "transfer_failed" }` and the call continues.

### 4.5 `send_dtmf` — emit DTMF toward the caller

```json
{ "type": "send_dtmf", "call_id": "...", "digit": "1", "duration_ms": 200 }
```

`digit` is one of `0-9 * # A B C D`. SiphonAI generates an RFC 2833
event of `duration_ms` (clamped to `[40, 2000]`).

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

WebSocket ping/pong is enabled. SiphonAI sends a ping every 15 s; if a
server fails to pong within 10 s, SiphonAI emits
`error { code: "internal", message: "ws keepalive timeout" }` and tears
down. Servers MAY ping SiphonAI; SiphonAI always pongs.

### 5.7 WS disconnect mid-call

If the WS connection closes (cleanly or otherwise) before SiphonAI has
sent `stop`, SiphonAI:

1. Stops sending audio frames over the (now-closed) WS.
2. Plays the configured `bridge.fallback_prompt_path` audio file (or
   silence) into the call.
3. Sends SIP BYE.
4. Emits a CDR with `stop_reason = "ws_disconnect"`.

WS reconnect mid-call is **post-v1** and not supported in this version.

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
