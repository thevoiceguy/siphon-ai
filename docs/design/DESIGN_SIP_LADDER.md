# Design note — per-call SIP ladder in sightglass

Status: **proposed**. Requested 2026-08-19: *"on the calls tab can we
select a call and see the SIP messaging for it?"*

Companion to `DESIGN_SIGHTGLASS.md`, which this extends. Read §6 of
that note first — the three rings it already specifies
(`error_ring`, `cdr_ring`, the stats snapshot) are the pattern this
one follows.

---

## 0. Scope, stated up front

This is a **nice-to-have**. It answers "what did this call's signaling
look like?" without leaving the terminal, for the case where you are
already looking at the call in sightglass and want the shape of the
exchange — which leg sent the re-INVITE, what the 4xx actually was,
whether the ACK arrived.

**It is not a replacement for HEP/Homer, and the docs must say so.**
Homer has permanent retention, cross-node correlation, search by any
header, RTCP and CDR chunks stitched into the same view, and a real
ladder diagram. Any in-depth troubleshooting belongs there. This ring
holds minutes of history for calls on one node and is the first thing
to drop if it ever costs anything.

That framing drives every bound below. When a choice is between
"cheap" and "complete", this note picks cheap and points at Homer.

---

## 1. The finding that makes this tractable

**Every SIP message for a call already passes through the daemon's own
process, in exactly the form the feature needs.**

siphon-rs's `sip-hep` does not own a transport. It emits into a
`HepSinkHandle` — `Arc<dyn HepSink>` — that *siphon-ai* constructs and
hands down (`bins/siphon-ai/src/runtime.rs`, `.with_hep_telemetry(…)`;
the comment there already notes the SIP/RTCP/CDR emitters share one
worker). Each SIP message arrives as:

```rust
HepPacket {
    protocol: HepProtocol::Sip,
    correlation_id: Some(call_id),   // the SIP Call-ID
    src, dst,                        // SocketAddrs, so direction is derivable
    timestamp: SystemTime,
    payload: Vec<u8>,                // the raw message bytes
    ..
}
```

So there is nothing to capture that is not already captured, no
siphon-rs change, no second parse of the wire, and no new dependency.
The only question is where the bytes are held and who may read them.

`HepSink` has a single method, `fn send(&self, packet: HepPacket)`, and
the handle is an `Arc<dyn …>` — so **a fan-out sink is a dozen lines**,
which is what §3 uses to decouple this from HEP being configured.

---

## 2. Decisions taken (2026-08-19)

| Question | Decision |
|---|---|
| Who can read it | **`operator` or `admin`.** `readonly` cannot. |
| Redaction | **None.** The full message, including `Authorization` / `Proxy-Authorization` and all headers. |
| Retention | **Match the CDR ring** — live calls, plus the last `cdr_ring_size` (default 50) completed ones. |
| Enabled by default | **Yes**, at 50. `sip_ring_size = 0` turns it off. See §6. |
| Positioning | Nice-to-have; deep troubleshooting is HEP/Homer's job. |

On redaction: the deliberate choice is that a role trusted to hang up
a live call and originate billable outbound is trusted to read a
digest header. A partial message is worse than no message — the whole
point is seeing what actually went over the wire, and a redacted
ladder invites the wrong conclusion. The access boundary is the token
role, not the message content. `DEPLOY.md`'s admin-auth section gets a
sentence saying so, because it changes what an `operator` token is
worth if leaked.

---

## 3. Architecture

```
siphon-rs sip-hep ─┐
forge-media rtcp  ─┼─► HepSinkHandle (fan-out) ─┬─► UDP HEP worker → Homer   [if [observability.hep]]
siphon-ai logs/CDR ┘                            └─► sip_ring (this note)     [if [observability].sip_ring_size > 0]
```

### 3.1 The ring is not coupled to HEP shipping

Today `hep_telemetry` is an `Option<Arc<HepTelemetry>>`, built only
when `[observability.hep]` is configured. Teeing naively off the
existing UDP sink would make this feature **silently do nothing** on a
node with HEP off — the worst failure mode for an observability
feature, because the pane looks empty rather than unavailable.

Instead: when either HEP shipping **or** the SIP ring is enabled,
build a `HepTelemetry` whose sink is a fan-out over the enabled
destinations. With HEP off and the ring on, the fan-out has exactly
one leg and no UDP socket is opened. With both off, `hep_telemetry`
stays `None` exactly as today and no `HepPacket` is ever constructed.

The cost when the ring is on and HEP is off is that `HepPacket`s get
built for messages that would otherwise never be built — a `Vec<u8>`
per SIP message. SIP is **not the audio path** (CLAUDE.md §4.3): a
call is a handful of messages over its lifetime, against 50 audio
frames/sec. This is nowhere near the hot path and needs no pooling.

### 3.2 Ring shape

`crates/telemetry/src/sip_ring.rs`, mirroring `cdr_ring.rs`:
process-global behind a `OnceLock<Mutex<…>>`, so a SIGHUP sink rebuild
does not lose history, with `set_capacity` on config load/reload.

Keyed by **SIP Call-ID** (what `correlation_id` carries), holding per
call an ordered `Vec` of:

```rust
struct SipMessage {
    timestamp: SystemTime,
    src: SocketAddr,
    dst: SocketAddr,
    payload: String,        // lossy UTF-8; SIP is text
}
```

**Correction (implementation, 2026-08-19): there are three bounds, not
two.** This section originally assumed every trace is a call, so the
completed-call window would eventually evict all of them. It does not:
**the SIP stream carries far more than calls.** REGISTER refreshes,
OPTIONS pings, unsolicited NOTIFYs and — on any public-IP node — a
steady drip of scanner INVITEs rejected with 403 all carry a `Call-ID`
and all reach this sink. None becomes a call, so none emits a CDR, so
none is ever *completed*, so none would ever be evicted. The reference
node alone would feed it ~1,440 REGISTER cycles and ~150 rejected
INVITEs a day, forever.

Filtering to "is it a call?" at capture time is not available either:
the INVITE arrives *before* the call exists in any registry, and that
first message is the one most worth having. So traces live in two
populations — **completed** (a CDR was emitted; capped at `cap_calls`,
evicted oldest-first) and **pending** (live calls plus all of the
above; capped at `MAX_PENDING` = 256, evicted least-recently-touched).
`MAX_PENDING` is deliberately not configurable: it is a backstop, not
a tuning knob, and an operator who wants less should set
`sip_ring_size = 0`.

The three bounds:

- **Per call: 64 messages** (`sip_ring_max_messages`). A normal call is
  6–20. 64 covers re-INVITE churn, auth retries and a REFER without
  letting one pathological dialog — a retransmit storm, a glare loop —
  evict every other call's history. On overflow, drop the **oldest**
  within that call and set a `truncated` flag the endpoint reports, so
  the UI can say so instead of lying by omission.
- **Across calls: `cdr_ring_size` completed calls** (default 50), plus
  every live call unconditionally. A call's entry is retained on
  hangup and evicted when it falls off the completed-call window —
  which is precisely the retention the recent-calls pane already has,
  so the two panes never disagree about which calls you can inspect.

Worst case at defaults is **~55 MB** — not the ~4.8 MB that
`cap_calls × 64 × ~1.5 KB` suggests, which is what an earlier revision
of this note published. That arithmetic counts only the completed
window and silently drops the two populations introduced immediately
above it. The real bound is `MAX_PENDING (256) + MAX_LIVE (512) +
cap_calls (50)` = **818 traces**, each up to `sip_ring_max_messages`
(64) messages, at a **measured ~1.05 kB per message**: an order of
magnitude more than the completed window alone implies.

It is still a bound rather than an expectation, and not one reached by
accident — it needs 512 concurrent calls *each* having exchanged 64+
messages, where real calls carry 4–8. The figures that describe a
running node are far smaller: **~1.9 MB at a realistic 200-concurrent
shape**, about **2.5 %** of the 77 MB that node's whole daemon
occupied at that load, and **~17 MB** with the pending population
saturated at the per-call cap. Per-message cost is roughly `1.34 ×
payload + 285 B`, the slope above 1.0 being allocator size-class
rounding rather than bookkeeping — so the total scales linearly with
`sip_ring_max_messages`, which is the knob to reach for if it ever
matters. Measured on the shipped 0.49.7 binary in
`test-harness/load/RESULTS-0.49.7-ring-ab.md`. `0` disables.

### 3.3 The id join

`/admin/v1/calls/…` speaks siphon-ai's internal `call_id`; the ring is
keyed by SIP Call-ID. `AdminCallRow` already carries **both**
(`call_id`, `sip_call_id`), so the handler resolves internal → SIP via
the registry and looks up the ring. For a completed call the same pair
is in the CDR ring record. No new state, no third id namespace.

---

## 4. Endpoint

```
GET /admin/v1/calls/{id}/sip        →  200 | 401 | 403 | 404 | 501
```

`{id}` is the internal call id, matching its `/stats`, `/hangup`,
`/park` siblings. Response:

```json
{
  "call_id": "01J…",
  "sip_call_id": "a84b4c76e66710@pbx.example.com",
  "truncated": false,
  "count": 7,
  "messages": [
    { "timestamp": "2026-08-19T00:32:30.777Z",
      "direction": "out",
      "src": "139.177.205.140:5060",
      "dst": "194.195.208.34:5060",
      "payload": "REGISTER sip:… SIP/2.0\r\n…" }
  ]
}
```

`direction` is `"in"` / `"out"` / `"unknown"`, derived by matching
`src` and `dst` **on IP** against the node's own addresses
(`[node].public_address`, plus the SIP bind IP when it is not a
wildcard). `src`/`dst` are kept so the derivation is always checkable.

**Correction (implementation):** the obvious version of this — match
the source *port* against our bind, treating a `0.0.0.0` bind as
"any IP" — is wrong, and wrong in the common direction. SIP peers
overwhelmingly send *from* 5060 as well, so a port-based test labels
almost every inbound message `"out"`. A unit test caught it before it
shipped and now locks it.

**Second correction (0.49.4, found by running the 0.49.3 artifact
against a production-shaped node):** matching *only* the configured
addresses was also wrong, and broke the field on the deployment shape
it was built for. siphon-rs stamps a HEP packet's local end with the
**socket's** address, so on the usual `listen = "0.0.0.0:5060"` our own
end is literally `0.0.0.0` — proven from prod's Homer capture:
inbound `srcIp <peer> / dstIp 0.0.0.0`, outbound the reverse. Neither
end matched the configured public address, so **every message on a
wildcard-bound node rendered `"unknown"`** and sightglass drew no
arrows at all. On loopback the opposite: both ends are `127.0.0.1`,
both "ours", and everything read `"out"`.

So: an unspecified IP counts as ours (that is exactly what siphon-rs
means by it), and when *both* ends look local the SIP bind port breaks
the tie — port consulted only after IP has failed, never before, which
is what keeps the first correction true. Where neither end is
recognisably this node the value stays `"unknown"`: guessing would be
worse than saying so, and the client has `src`/`dst`.

Messages are **oldest first** — a ladder reads down the page in wire
order, unlike the newest-first `errors` / `cdrs` listings — and
`ts_ms` matches `ErrorEntry`'s encoding rather than this note's
original RFC3339 sketch, so the ring endpoints share one time format
and a client can subtract them.

`404` is an unknown id; `200` with an empty list means the call is
known but its trace was never captured or has been evicted — the two
are distinguished because the answer to "why is this empty?" differs.

`501` when `sip_ring_size = 0`, matching how the conference and park
endpoints report a disabled feature; sightglass must degrade to a
"disabled" note and **must never mark the node down** for it — the
same discipline §6.1 of the sightglass note set for pre-0.49 daemons.

Wire shapes go in `crates/admin-api-types/` with snapshot tests, per
CLAUDE.md's table.

### 4.1 RBAC — one genuinely new thing

This is the **first `GET` on the admin API gated above `readonly`**.
Every existing read is readonly; `Operator` has so far meant "may
change something". `auth.rs::min_role` handles parameterised routes
through `operator_pattern`, which today only matches mutating verbs —
so this needs a deliberate arm, not a pattern tweak, and a test that
asserts a `readonly` token gets **403 on GET …/sip while still getting
200 on GET …/stats**. That test is the one that stops a future
refactor from quietly widening the read surface.

Sightglass consequence: its startup role probe already learns each
node's role, so with a `readonly` token the ladder pane renders
"requires operator" rather than empty or errored.

---

## 5. Sightglass UI

`bins/sightglass/src/ui/calls.rs` (381 lines) already renders a
focused-call detail pane beside the fleet table. The ladder is a third
region in that pane, or a full-height overlay toggled with **`s`**:

```
┌ call 01J… ── 139.177.205.140 → +1555… ────────────── 7 msgs ┐
│ 00:32:30.777  →  INVITE sip:+1555…@… (SDP 214 B)            │
│ 00:32:30.778  ←  100 Trying                                 │
│ 00:32:30.812  ←  407 Proxy Authentication Required          │
│ 00:32:30.813  →  ACK                                        │
│ 00:32:30.814  →  INVITE  (auth)                             │
│ 00:32:31.204  ←  200 OK (SDP 198 B)                         │
│ 00:32:31.205  →  ACK                                        │
└ ⏎ expand · j/k scroll · y copy · s close ────────────────────┘
```

- One line per message: relative-to-call timestamp, direction arrow,
  start-line, and a size hint for bodies.
- `⏎` expands one message to full raw text; `y` copies it.
- Polled with the rest of the focused call's detail, on the existing
  per-node poller — **only while the ladder is open**, since it is the
  one payload here that is kilobytes rather than tens of bytes.
- Node-scoped like every other pane; a node that 501s shows the
  disabled note in its own row.

---

## 6. Configuration

```toml
[observability]
sip_ring_size         = 50   # completed calls retained; 0 disables. Default: cdr_ring_size
sip_ring_max_messages = 64   # per-call cap
```

Defaulting `sip_ring_size` to `cdr_ring_size` is what keeps the
promise in §2 without asking the operator to keep two numbers in sync.
Both validated at load (CLAUDE.md §4.6): >65536 fails loud, as
`error_ring_size` already does.

**Default: on, at 50** (decided 2026-08-19). A feature that needs
enabling before the incident is a feature that is off during the
incident, and this one's whole value is being already-populated when
you go looking.

The accepted cost is that every node then holds recent SIP — including
the `Authorization` headers §2 chose not to redact — in process
memory by default. Two consequences the implementation must carry
rather than leave implicit:

- **`DEPLOY.md` states it plainly**, in the same place it documents
  admin auth: on a default install, `operator` is a role that can read
  recent credentials, and the ring is what makes that true. An operator
  who does not want that sets `sip_ring_size = 0`, which is a
  documented, supported configuration and must stay one.
- **The ring holds no more than the process already does.** These
  bytes were in the daemon's address space anyway — as the parsed
  dialog, and as the `HepPacket` shipped to Homer wherever HEP is
  enabled. Defaulting on extends their *lifetime* (minutes, bounded by
  §3.2) and their *reachability* (an authenticated operator endpoint).
  It does not introduce a class of data the node was not already
  handling, and it writes nothing to disk — which is the line that
  keeps this different from per-call log files (CLAUDE.md §8).

Anyone whose threat model dislikes that turns it off with one key, and
the 501 path in §4 means sightglass reports it as disabled rather than
broken.

---

## 7. Observability (CLAUDE.md §4.5 — ships in the same PR)

- `siphon_ai_sip_ring_messages_total{result="captured"|"dropped_call_cap"|"dropped_trace_cap"}`
  — a rising `dropped_call_cap` is the signal that 64 is wrong, or that
  something is retransmitting. `dropped_trace_cap` (added with the
  pending bound above) counts whole dialogs evicted, i.e. REGISTER /
  scanner noise crowding out live calls.
- `siphon_ai_sip_ring_traces` gauge — dialogs currently held. Named
  `traces`, not the note's original `calls`, for the same reason the
  third bound exists: most of what it holds on a quiet public node is
  not a call.
- No new logs on the capture path: it is a mutex push per SIP message,
  and a log line per SIP message is the noise class 0.49.2 just spent a
  release removing.
- No CDR field, no webhook, no protocol change. The WS protocol is
  untouched — this is admin-side only.

---

## 8. Non-goals

- **Not a Homer replacement.** No search, no cross-node correlation,
  no permanent retention, no RTCP or CDR chunks in the view.
- **Not a capture toggle.** No per-call "start tracing" control; the
  ring is always-on or off by config.
- **Not RTP/RTCP.** Signaling only. Media quality already has the
  stats pane and Homer's QoS chunks.
- **No export.** `y` copies one message to the clipboard; anything
  larger is a `curl` of the endpoint, or Homer.
- **Not per-call log files** — explicitly out of scope per CLAUDE.md §8.

---

## 9. Build plan

Two PRs, in order. Neither is large.

1. **Daemon** — `sip_ring.rs`, the fan-out sink, the two config knobs
   with validation, `GET /admin/v1/calls/{id}/sip`, the `min_role`
   arm, wire types in `admin-api-types` with snapshot tests, metrics,
   `CONFIG.md` + `DEPLOY.md`. Testable end-to-end with `curl` alone.
2. **Sightglass** — the ladder pane, `s`/`⏎`/`y` keys, the 501 and
   403 degradations, `SIGHTGLASS.md`.

## 10. Testing

- Ring unit tests: per-call cap evicts oldest **within** the call and
  sets `truncated`; completed-call window evicts whole calls in
  hangup order; live calls are never evicted; `0` disables.
- The RBAC test in §4.1 — `readonly` gets 403 on `…/sip` and 200 on
  `…/stats` in the same test.
- An integration test driving a real SIPp scenario through
  `test-harness/` and asserting the ring holds the exact message
  sequence, including the 407 retry — the auth retry is the case worth
  locking, since it is both the most common real ladder and the one
  carrying the credentials §2 decided not to redact.
- A HEP-off test: ring populated with `[observability.hep]` absent,
  proving §3.1 and that no UDP socket is opened.
