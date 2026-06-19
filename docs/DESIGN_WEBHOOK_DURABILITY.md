# Design: Webhook / CDR delivery trust + durability

Status: **DRAFT — decisions pending** (this note + an AskUserQuestion lock
the forks, then chunked PRs, same cadence as admin-auth → v0.10.0).

Theme: P0 from the post-delivery-plan list — *"Webhook/CDR delivery needs
production-grade trust and durability. Add `X-SiphonAI-Signature`, event
IDs/idempotency keys, durable retry spool, and delivery metrics."*

---

## 1. The gap today

Both outbound HTTP surfaces — lifecycle webhooks (`siphon-ai-webhooks`) and
the CDR webhook sink (`siphon-ai-cdr`) — are thin adapters over one shared
transport, `siphon-ai-http::RetryingPoster`:

- **Fire-and-forget**: each `emit` `tokio::spawn`s the POST so the call path
  never blocks (correct, keep it).
- **In-memory retry only**: transient failures (5xx/408/429/connect) retry
  with exponential backoff (100ms→5s cap), then the payload is **logged and
  dropped**. A daemon restart loses every in-flight retry.
- **No authenticity**: the body is unsigned. A receiver can't distinguish a
  genuine SiphonAI POST from a spoofed one; the only knob is a verbatim
  `Authorization` header (a shared bearer, not per-message proof).
- **No idempotency**: a retry (or a future spool replay) is byte-identical
  with no stable id, so a receiver that got the body but failed to ACK will
  double-process on retry.
- **No delivery observability**: no metric for delivered / dropped / spool
  depth. Failures live only in `warn!` logs.

The transport's own doc comments already name all of this as the intended
home for the fix ("future HMAC request signing… belongs in `siphon-ai-http`
so both webhook sinks gain it at once"). This theme delivers it.

## 2. Goals / non-goals

**Goals**
1. **Authenticity** — `X-SiphonAI-Signature` HMAC over the body so receivers
   can verify the POST came from us and wasn't tampered/replayed.
2. **Idempotency** — a stable per-event delivery id so retries/replays are
   safely de-dupable by the receiver.
3. **Durability** — a disk-backed spool so deliveries survive a restart and
   keep retrying, instead of being dropped after the in-memory budget.
4. **Delivery metrics** — counters/gauge/histogram for delivered / dropped /
   spooled / spool-depth, with bounded-cardinality labels.

**Non-goals (this theme)**
- Changing *when* events fire or the event/CDR field schemas beyond what
  idempotency strictly needs (see §5 decision).
- A general durable message queue / exactly-once delivery. Spool gives
  **at-least-once** with restart survival; the idempotency key makes
  at-least-once safe for receivers. That's the contract.
- Per-receiver fan-out / multiple webhook URLs. Still one URL per sink.
- mTLS to the receiver (orthogonal; `[bridge.tls]`-style client certs are a
  separate ask).

## 3. Design

Everything lands in **`siphon-ai-http`** (the shared transport) plus config
wiring in the two sink crates + the daemon, so lifecycle webhooks and the
CDR webhook gain all four properties at once. The call path stays
fire-and-forget; all new work (disk I/O, signing, spool drain) is off-path.

### 3.1 Signature (`X-SiphonAI-Signature`)

A per-sink optional `secret`. When set, every attempt computes
HMAC-SHA256 and adds headers (exact wire format = **decision 1**):

- *Stripe-style* (recommended): `X-SiphonAI-Signature: t=<unix>,v1=<hex>`
  where the signed string is `"<unix>.<raw-body>"`. Timestamp is built in →
  receiver enforces a freshness window → replay protection without a second
  header.
- *GitHub-style*: `X-SiphonAI-Signature: sha256=<hex>` over the raw body,
  plus a separate `X-SiphonAI-Timestamp` header folded into the HMAC.

The signature + timestamp are computed **per send attempt** (a spool replay
hours later is a fresh, in-window send). The body bytes are serialized
**once** and both signed and sent, so the signature always matches the exact
wire bytes. Secret is stored from `${VAR}`-expanded config, never logged.

### 3.2 Idempotency key

A UUIDv4 generated **once when the event/CDR is enqueued** (not per attempt),
so every retry and every post-restart spool replay carries the **same** key.
Delivered as a header `X-SiphonAI-Event-Id` (and `Idempotency-Key` alias).
Body-envelope placement (a new `event_id`/`delivery_id` field on the webhook
+ CDR JSON) is **decision 3** — header-only avoids a schema/version bump;
header+body also serves receivers that only see the parsed body.

The key is part of the persisted spool envelope (§3.3) so it survives
restart. CDR records already have a unique `call_id`; lifecycle events do
not (e.g. `registration_state_changed` repeats), so the generated id is the
canonical idempotency unit for both.

### 3.3 Durable spool

A per-sink optional `spool_dir`. Persistence model = **decision 2**:

- *Spool-on-failure* (recommended): try the in-memory retry budget first
  (today's fast path, zero disk I/O on the happy path); only when that's
  exhausted, write the delivery to the spool. A background drain worker
  re-attempts spooled deliveries with backoff and, on success, unlinks the
  file. On startup the worker scans `spool_dir` and resumes — this is what
  buys restart survival. Gap vs write-ahead: a crash in the seconds-long
  first-delivery window loses that one delivery.
- *Write-ahead*: persist every delivery to disk **before** the first
  attempt, unlink on success. Strongest durability (survives a mid-first-
  attempt crash) at the cost of a disk write per webhook/CDR even when the
  receiver is healthy.

Spool envelope (one JSON file per delivery): `{ id, kind, url, created_at,
attempts, next_attempt_at, body }`. Filename `<created_ts>-<id>.json` for
stable oldest-first ordering. A permanent 4xx → move to `dead/` (or drop
with metric); the spool is size-capped (max files / bytes) → oldest dropped
with a metric tick when full (best-effort, never blocks, CLAUDE.md §4.7).
Default posture: **durability is opt-in** — no `spool_dir` ⇒ today's
best-effort behavior, unchanged.

### 3.4 Metrics (`crates/telemetry/src/metrics.rs`, emitted from `siphon-ai-http`)

- `siphon_ai_webhook_deliveries_total{sink, result}` — counter.
  `sink ∈ lifecycle|cdr`; `result ∈ delivered|spooled|dropped|rejected`
  (`rejected` = non-retryable 4xx; `dropped` = budget/spool exhausted).
- `siphon_ai_webhook_delivery_attempts_total{sink, outcome}` — counter of
  individual HTTP attempts (`outcome ∈ ok|transient|error|rejected`).
- `siphon_ai_webhook_spool_depth{sink}` — gauge, current spooled file count.
- `siphon_ai_webhook_delivery_seconds{sink}` — histogram, accepted→delivered
  latency (covers spool dwell so operators see backlog drain time).

`sink` label is bounded (two values). No `call_id` label (use logs / the
idempotency id for per-delivery detail), per the §4.5 cardinality rule.

## 4. Config surface (proposed)

Per-sink, all optional and additive (omitting them = today's behavior):

```toml
[webhooks]
# … existing url / auth_header / events / retry_max / timeout_ms …
secret    = "${WEBHOOK_SECRET}"          # enables X-SiphonAI-Signature
spool_dir = "/var/lib/siphon-ai/spool/webhooks"   # enables durable spool

[cdr.webhook]
# … existing url / auth_header / retry_max / timeout_ms …
secret    = "${CDR_WEBHOOK_SECRET}"
spool_dir = "/var/lib/siphon-ai/spool/cdr"
```

Load-time validation (CLAUDE.md §4.6): a `spool_dir` that can't be
created/written → fail at startup, not at first failed delivery. Signing
secrets are `${VAR}`-expanded and never logged.

## 5. Decisions — LOCKED (2026-06-19)

1. **Signature format = Stripe-style** `X-SiphonAI-Signature: t=<unix>,v1=<hex>`
   over `"<unix>.<body>"`. Built-in replay window; recomputed per attempt.
2. **Spool model = spool-on-failure.** In-memory retry first (zero-disk happy
   path), persist only after the budget is exhausted, drain worker resumes on
   restart. Matches the best-effort/IO-light hot-path philosophy; CDRs also
   have the JSONL file sink as a second durable record.
3. **Idempotency = header-only** `X-SiphonAI-Event-Id` (+ `Idempotency-Key`
   alias). No webhook/CDR body change → **no schema version bump**. The id is
   a delivery-envelope concern, alongside the signature.

(Per-sink `secret`/`spool_dir`, opt-in durability default, at-least-once +
idempotency contract, and metric names are taken as defaults.)

## 6. Chunks (target ~v0.11.0)

1. **Trust**: `secret` config + `X-SiphonAI-Signature` + idempotency id +
   delivery metrics, all in `siphon-ai-http`; wire `[webhooks]`/`[cdr.webhook]`
   config; unit tests (sig vectors, stable id across retries). No spool yet —
   purely additive, no behavior change when `secret` unset.
2. **Durability**: `spool_dir` config + spool write/drain worker + restart
   recovery + spool-depth/dropped metrics + size cap + load-time validation;
   unit/integration tests (failed delivery spools; restart drains; cap drops).
3. **Docs + release**: CONFIG `[webhooks]`/`[cdr.webhook]` + DEPLOY delivery/
   security + a receiver-verification snippet (verify the signature) + metrics
   table; CHANGELOG; tag ~v0.11.0. Fold receiver examples into `examples/`.
