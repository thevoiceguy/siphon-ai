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
| `tracing` spans                     | Per-call latency between `on_invite` → `on_matched` → `accept_inbound` → `start_session`. With `[observability.otlp]` (0.22.0) these export as one OTLP trace per call (root carries the SIP `Call-ID`, direction, from/to) to Tempo / Jaeger — and the trace context propagates to the developer's WS server via `traceparent` on the WS upgrade + `start.trace_context` (0.23.0), so a server that continues it appears in the same waterfall. |
| Homer UI (`http://.../`)            | SIP call flow + RTCP + CDR + log chunks correlated by SIP `Call-ID`                    |
| Prometheus `/metrics`               | Counters / histograms; `siphon_ai_*` for app-level, `forge_*` for media, `heplify_*` for collector  |
| Grafana dashboards                  | Fleet Overview + Call Quality — shipped in [`examples/observability/`](../examples/observability/) (rates, ratios, latency percentiles) |
| CDR (file / webhook)                | One JSON record per ended call with codec, route, termination cause, durations         |
| `/admin/*`                          | Live state — active calls, registration status, log filter, HEP probe, drain status     |

## Dashboards & alerts as code

[`examples/observability/`](../examples/observability/) ships a runnable
Prometheus + Grafana stack — recording rules, alerting rules, and two Grafana
dashboards — so the metrics below come with visualizations and pages out of
the box (`docker compose -f examples/observability/compose.yaml up`). Where a
question below is answerable from metrics, the worked PromQL and the panel /
alert that covers it are called out. **Prometheus/Grafana for the aggregate
(rates, ratios, latency percentiles); Homer for the individual call** (SIP
flow + per-stream RTP quality); the daemon log for the per-call *why*.

Symptom → where to look first:

| Symptom | Dashboard / alert | Then |
|---|---|---|
| Callers turned away | `SiphonAIHighInviteRejectRate`; Fleet → *INVITE rate by outcome* | break down `siphon_ai_invites_total` by `result`; Q1 |
| Dead air at answer | `SiphonAISlowWsConnect`; Call Quality → *WS connect latency* | WS server health; Q5 |
| Choppy / laggy audio | `SiphonAIHighRtpRtt` / `SiphonAIHighPacketLoss`; Call Quality → RTP panels | Homer RTP-QoS per leg; Q6 |
| Not receiving calls | `SiphonAIRegistrationDown` / `SiphonAINoInboundCalls` | Q10; upstream trunk |
| Events not reaching a SIEM/billing | `SiphonAIWebhookSpoolBacklog` / `SiphonAIWebhookDeliveryFailing`; Fleet → delivery panels | receiver health |
| Rough deploy | `SiphonAIDrainForced` | lengthen `[shutdown].drain_timeout_secs` |

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

The shipped rules pre-compute the percentiles — query the recorded series
(no `histogram_quantile` in the panel):

```promql
siphon_ai:ws_connect_seconds:p99      # bridge to WS handshake + start
siphon_ai:sdp_negotiate_seconds:p99   # SDP parse + port alloc + tap attach
siphon_ai:call_duration_seconds:p95
```

The **Call Quality** dashboard (`examples/observability/`) plots all three.
Raw form if you need an ad-hoc quantile:
`histogram_quantile(0.99, sum by (le) (rate(siphon_ai_ws_connect_seconds_bucket[5m])))`.

### 5. Was the WS server slow to respond?

**Source:** `siphon_ai_ws_connect_seconds` histogram + bridge connection log.

```text
INFO connect_and_run{call_id=… ws_url=ws://echo-ws:8765/}:
   siphon_ai_bridge::conn: bridge connected
```

The histogram captures the time from `connect_and_run` start to
"bridge connected." For per-call rather than aggregate, subtract
log timestamps. The `SiphonAISlowWsConnect` alert fires on
`siphon_ai:ws_connect_seconds:p99 > 1` for 10m — a slow WS server (or the
network to it) means callers hear dead air at answer.

**Closed (0.30.0):** the CDR now carries
`quality.first_audio_out_ms` (CDR v4) — the time from "WS `start` on
the wire" to the first audio frame from the WS server reaching playout
toward the caller. That's the metric operators want for "how slow is
my STT/LLM/TTS chain at first token"; pair it with
`siphon_ai_ws_connect_seconds` to separate connect time.

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

For the aggregate view, SiphonAI's own histograms drive the **Call Quality**
dashboard and two alerts:

```promql
siphon_ai:rtp_rtt_ms:p95              # SiphonAIHighRtpRtt        (> 300ms/10m)
siphon_ai:rtp_packet_loss_ratio:p95   # SiphonAIHighPacketLoss    (> 5%/10m)
```

These tell you *a fleet-wide quality problem is happening*; drop to Homer's
per-leg RTP-QoS panel to see *which stream*.

**Per-call quality over time — the middle layer (0.30.0/0.31.0):**
between fleet aggregates and single-call Homer forensics sits the
question "chart *this call's* (or *every call's*) quality over its
lifetime, in my own dashboards." Three feeds, one shape (the CDR
`quality` block — MOS estimate, RX loss/reorder counters, RR jitter/loss
aggregates, `first_audio_out_ms`, `barge_in_count`):

1. **Live** — `GET /admin/v1/calls/{id}/stats` (readonly role): what one
   active call measures *right now*.
2. **History** — `[quality]` records (0.31.0): one JSON record per call
   per `interval_secs` + a final summary, to a JSONL file and/or an
   HMAC-signed webhook. Ingestion pipeline (reference stack in
   `examples/observability/`, runs with one `docker compose up`):

   ```text
   siphon-ai [quality.webhook] ──HTTP POST (signed, spooled)──► Vector :9411
                                                                   │
        [quality.file] quality.jsonl ──(alt: Vector file source)───┤
                                                                   ▼
                                             Loki (label job="siphon-quality",
                                                   kind=interval|final)
                                                                   ▼
                                  Grafana "Per-Call Quality History" dashboard
   ```

   Only `kind` becomes a Loki label — `call_id` stays a JSON field
   (per-call label values would explode the index; LogQL filters it via
   `| json | call_id=~"..."`). Counters are cumulative per call; use
   `max_over_time`-style unwraps rather than `rate` for staircase reads.
   The daemon's spool (`[quality.webhook].spool_dir`) rides out Vector /
   Loki restarts with no record loss.
3. **Post-mortem** — the CDR `quality` block (v4): the final record's
   numbers in the record you already ingest for billing.

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

**Closed (0.30.0):** the CDR now includes
`quality.barge_in_count` (CDR v4) — `auto_clear` firings plus
server-sent `clear` commands over the call's lifetime, so "did this
call barge in, and how often" reads straight from the record.

### 9. Did the WS server send an unexpected message?

**Source:** Daemon log, DEBUG level on `siphon_ai_bridge`.

```text
DEBUG siphon_ai_bridge::conn: ws inbound control parsed=Clear { call_id: … }
```

The bridge logs every parsed `BridgeIn` control message at
`debug` and every inbound audio frame at `trace`. Both are off
at default INFO; flip them at runtime via:

```sh
# /admin/* is on the [admin] listener (0.10.0), not the metrics port;
# PUT /admin/log needs an `admin`-role bearer token.
curl -X PUT --data 'siphon_ai=info,siphon_ai_bridge=debug' \
   -H "Authorization: Bearer $SIPHON_ADMIN_ADMIN" \
   http://127.0.0.1:9092/admin/log
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
tracks the current row of every `[[register]]` block; the shipped
`SiphonAIRegistrationDown` alert fires when a name has not been in the
`registered` state for 5m (`max by (name) (siphon_ai_register_state{state="registered"}) < 1`),
and the **Fleet Overview** dashboard shows the registered count. Inbound
calls to a non-registered AOR won't arrive, so this is a `critical`.

The question's premise belongs to a B2BUA model where one
mis-routed call could break a downstream registration. SiphonAI
is a UAS endpoint — calls and registrations live on independent
paths.

## Summary of identified gaps (follow-ups)

| Question | Gap                                                          | Sketch of fix                                                       |
|----------|--------------------------------------------------------------|---------------------------------------------------------------------|
| Q3       | `call ended` log doesn't include the SIP terminator method   | Add `sip_terminator="BYE"` field to the `acceptor::call_ended` log |
| Q5       | ~~No `first_audio_out_ms` in CDR~~ **CLOSED 0.30.0** — CDR v4 `quality.first_audio_out_ms` | Shipped: WS-connect instant → first server audio frame at playout |
| Q8       | ~~No `barge_in_count` in CDR~~ **CLOSED 0.30.0** — CDR v4 `quality.barge_in_count` | Shipped: `auto_clear` firings + server `clear` commands per call |

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
