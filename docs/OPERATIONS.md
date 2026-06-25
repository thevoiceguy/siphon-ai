# Operating SiphonAI in production

This doc answers the operability bar from `docs/DEV_PLAN.md` §11.8:
**every routine diagnostic question should be answerable from
logs + traces + HEP alone**, without attaching a debugger to a
running daemon.

The audit was last run on **2026-05-12** against a clean call
flowing through the daemon docker stack + the Homer demo stack
(`examples/homer-stack/`). Each entry below cites the concrete
evidence and flags any gaps.

## Quick reference — where to look first

| Source                              | What's there                                                                           |
|-------------------------------------|----------------------------------------------------------------------------------------|
| Daemon log (stdout / journald)      | Lifecycle events, state transitions, errors, registration drive output                 |
| `tracing` spans                     | Per-call latency between `on_invite` → `on_matched` → `accept_inbound` → `start_session` |
| Homer UI (`http://.../`)            | SIP call flow + RTCP + CDR + log chunks correlated by SIP `Call-ID`                    |
| Prometheus `/metrics`               | Counters / histograms; `siphon_ai_*` for app-level, `forge_*` for media, `heplify_*` for collector  |
| CDR (file / webhook)                | One JSON record per ended call with codec, route, termination cause, durations         |
| `/admin/*`                          | Live state — active calls, registration status, log filter, HEP probe, drain status     |

## The §11.8 ten questions

### 1. Why did this call hit this route?

**Source:** Daemon log, INFO level.

```text
INFO on_invite{method="INVITE" peer=172.21.0.1:37014}:
   siphon_ai_sip_glue::handler: INVITE routed
   route="default" from_user="sipp" request_uri_user="1000"
   register_source="trunk"
```

The `INVITE routed` line carries every field the route grammar
evaluates (`from_user`, `request_uri_user`, `register_source`)
plus the winning route's `name`. A 404 (no route matched) emits
the same fields under `INVITE rejected: no route matched`.

### 2. Why was this codec chosen?

**Source:** Daemon log, INFO level.

```text
INFO siphon_ai_media_glue::setup: inbound call media setup complete
   negotiated=PCMU sample_rate=8000 rtp_port=40078
```

The full offer SDP appears verbatim inside the `create_session`
span at the same level (so the format list is visible). The
negotiator picks the offer's *first* format that our caps
support, per RFC 3264 §6.1 — that's the explanation for "why
this codec." Mid-call codec change would log a fresh
`media setup complete` line.

### 3. Why did the call drop?

**Source:** Daemon log, INFO level. Answer is in two correlated lines:

```text
INFO on_bye{method="BYE"}: siphon_ai_sip_glue::dialog:
   BYE → controller shutdown sip_call_id=1-2651348@127.0.0.1
…
INFO siphon_ai_core::acceptor: call ended
   call_id=siphon-6ce27797cc0a4997b90cbae2f46ce7a4
   cause=LocalShutdown
```

The pair maps the SIP-level signal (BYE / CANCEL / WS-driven
hangup) onto the controller-level termination cause. The
correlation key is the SIP `Call-ID`. The `cause` enum is one of
`ServerHangup` (WS sent `hangup`), `LocalShutdown` (SIP-side BYE
or CANCEL signalled the controller), `BridgeEnded` (WS closed
first), `TapEnded` (forge tap stopped).

**Gap (small):** The terminal `call ended` line doesn't echo the
SIP method that drove the shutdown. Today operators correlate
via `call_id` between the two lines. Could be tightened by
adding `sip_terminator="BYE"` to the terminal line — filed as a
follow-up since it's a string field, not a behaviour change.

### 4. Where was the latency?

**Source:** Daemon log timestamps (nanosecond precision) + tracing spans.

Every per-call function is `#[instrument]`-ed. The span
hierarchy on inbound INVITE is:

```text
on_invite { peer, method }
  └─ on_matched { route, from, to }
       └─ accept_inbound { call_id }
            └─ create_session { participant_a, participant_b, sdp, … }
                 └─ create_session_with_config { … }
```

Subtracting consecutive log timestamps yields per-stage latency.
For systemic latency tracking the daemon exposes:

- `siphon_ai_sdp_negotiate_seconds` (histogram, labels `result=ok|error`)
- `siphon_ai_ws_connect_seconds` (histogram)
- `siphon_ai_call_duration_seconds` (histogram of total call wall time)

Pipe to Grafana; `histogram_quantile(0.99, ...)` per stage gives
the headline numbers.

### 5. Was the WS server slow to respond?

**Source:** `siphon_ai_ws_connect_seconds` histogram + bridge connection log.

```text
INFO connect_and_run{call_id=… ws_url=ws://echo-ws:8765/}:
   siphon_ai_bridge::conn: bridge connected
```

The histogram captures the time from `connect_and_run` start to
"bridge connected." For per-call rather than aggregate, subtract
log timestamps.

**Gap:** The CDR doesn't carry `first_audio_out_ms` — the time
from `bridge connected` to the first audio frame from the WS
server reaching the caller. That's the metric operators
typically want for "how slow is my STT/LLM/TTS chain at first
token." Filed as a follow-up; needs a CDR schema bump (additive
optional field — no `version` bump required per the CDR
backwards-compat policy).

### 6. Did the caller experience audio quality issues?

**Source:** HEP3 RTP-QoS chunks in Homer + `forge_rtcp_*` metrics.

When a call exchanges real RTP (vs the no-audio SIPp scenario in
the audit run), `forge-media` emits:

- HEP3 chunk type **0x05** for every RTCP packet observed (SR/RR/SDES/BYE)
- HEP3 vendor chunk type **0x20** (`HepProtocol::RtpQos`) per RR
  with `ssrc`, `fraction_lost`, `cumulative_lost`, `jitter`

In Homer's call view these stitch onto the same call_id as the
SIP exchange, so quality troubleshooting goes "find the call,
look at the QoS panel."

Prometheus also exposes `forge_rtcp_packet_loss_fraction`,
`forge_rtcp_packets_lost_total` (gauges over the most recent
RR), and `forge_rtcp_jitter_ms` for dashboards.

### 7. What did the SIP exchange actually look like?

**Source:** Homer UI's Call Flow view.

Every parsed inbound and serialized outbound SIP message ships
to Homer via `siphon-rs::sip-hep`. In the demo run, one
basic_call_then_bye scenario produces ~7 HEP packets
(INVITE/100/200/ACK/BYE/200 + occasional retransmits), all
threaded by SIP `Call-ID`. Homer renders them as a ladder
diagram. For UDP / TCP / TLS / WS / WSS — all five transports
are hooked.

### 8. Did barge-in fire when expected?

**Source:** Daemon log, INFO level + tracing events.

```text
INFO siphon_ai_core::call: VAD: caller speech started → clearing playout
```

The events come from `forge-vad`'s `ForgeEvent::SpeechStarted` /
`SpeechStopped`, surfaced by `MediaTap` to the bridge. They
correlate with the WS server's `clear` messages emitted under
`auto_clear` mode.

**Gap:** The CDR doesn't include a `barge_in_count` — operators
who want to know "did this call barge in, and how often" have to
scrape logs. Filed as a follow-up CDR schema addition (same
backwards-compat note as Q5).

### 9. Did the WS server send an unexpected message?

**Source:** Daemon log, DEBUG level on `siphon_ai_bridge`.

```text
DEBUG siphon_ai_bridge::conn: ws inbound control parsed=Clear { call_id: … }
```

The bridge logs every parsed `BridgeIn` control message at
`debug` and every inbound audio frame at `trace`. Both are off
at default INFO; flip them at runtime via:

```sh
curl -X PUT --data 'siphon_ai=info,siphon_ai_bridge=debug' \
   http://127.0.0.1:9091/admin/log
```

The admin endpoint replies with the previous filter so an
incident can be exited cleanly. Trace-level audio logging is for
hardcore debugging only — at 50 frames/sec it floods even at
small call volumes.

### 10. Which call ended my registration?

**Status:** Doesn't map cleanly onto SiphonAI's registration model.

The `[[register]]` drive task is independent of any specific
call. It REGISTERs on startup, refreshes at `expires - 60s`, and
retries with exponential backoff on failure. Failures come from
network/auth/registrar 4xx/5xx, none of which are caused by a
call.

What we **do** log on every state transition:

```text
INFO crate::registration::drive{name=cucm-main}:
   registration succeeded granted_expires_secs=3600
…
WARN registration drive task ended with error error=…
```

The Prometheus gauge `siphon_ai_register_state{name="…", state="…"}`
tracks the current row of every `[[register]]` block; alert on
`failed` or `disabled` to catch outages.

The question's premise belongs to a B2BUA model where one
mis-routed call could break a downstream registration. SiphonAI
is a UAS endpoint — calls and registrations live on independent
paths.

## Summary of identified gaps (follow-ups)

| Question | Gap                                                          | Sketch of fix                                                       |
|----------|--------------------------------------------------------------|---------------------------------------------------------------------|
| Q3       | `call ended` log doesn't include the SIP terminator method   | Add `sip_terminator="BYE"` field to the `acceptor::call_ended` log |
| Q5       | No `first_audio_out_ms` in CDR                               | Track `Instant` from `bridge connected` to first WS binary frame; add additive optional field to CDR schema |
| Q8       | No `barge_in_count` in CDR                                   | Count `MediaTap::Clear` invocations per call; add additive field   |

None block the operability bar — all are answerable through
log scraping today; the follow-ups make them visible in
structured CDR JSON instead. The §11.8 acceptance is met as of
this audit.

## When to bump log level

Default filter: every `siphon_ai_*` crate at `info`, upstream
busy spans (`sip_uas`, `sip_transaction`, `sip_transport`,
`forge=warn`) silenced. Bump to `debug` for:

| Layer                | What you'll see                                                |
|----------------------|----------------------------------------------------------------|
| `siphon_ai_bridge`   | Every received WS message (control + audio counts)             |
| `siphon_ai_core`     | Call-controller state transitions + tap commands              |
| `siphon_ai_sip_glue` | Dialog dispatch, route-match details                          |
| `sip_uas`            | Inbound transaction lifecycle (Trying/Ringing/200/487)        |
| `sip_transaction`    | Per-transaction state machine (use only during SIP debugging) |
| `forge`              | Frame-by-frame RTP / VAD events (very chatty)                 |

All of the above are reachable without restart via
`PUT /admin/log`.

## Draining for a deploy / restart (0.17.0)

On `SIGTERM`/`SIGINT` the daemon drains instead of dropping calls: `/ready`
flips false, new INVITEs get `503`, in-flight calls finish (up to
`[shutdown].drain_timeout_secs`), then stragglers get a clean `BYE`. Full
behaviour + k8s/systemd setup is in `DEPLOY.md` → *Graceful shutdown &
rolling deploys*. To watch one happen:

```text
INFO drain started active_calls=7 timeout_secs=30
INFO draining remaining=3
INFO drain complete; all calls ended elapsed_secs=12.4
# …or, if calls outlived the window:
WARN drain timeout reached; force-terminating 2 straggler call(s)
INFO signalled drain-forced teardown (BYE + WS hangup); waiting for it to flush
```

| Want to know… | Look at |
|---------------|---------|
| Is this pod draining right now? | `siphon_ai_draining` gauge (1), or `GET /admin/v1/drain` `{draining, active_calls, remaining_secs}` |
| How long did the last drain take? | `siphon_ai_drain_seconds` histogram (near the timeout = calls didn't finish) |
| Are deploys force-killing calls? | `siphon_ai_calls_drain_forced_total` — if regularly non-zero, raise `drain_timeout_secs` (and the supervisor's kill grace) |
| Which calls were force-ended? | CDR `termination.cause == "drain_forced"` (CDR v3) |

If a drain is taking too long (an operator wants out *now*), send a **second**
`SIGTERM`/`SIGINT` — it forces immediate teardown, dropping whatever's left.
