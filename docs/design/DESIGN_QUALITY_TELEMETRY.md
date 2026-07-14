# Design: Per-call quality telemetry (live + history)

**Status: SHIPPED — v0.30.0 + v0.31.0 (2026-07-13/14). Retrospective in §8.**
Theme: ROADMAP P1 "Per-call quality telemetry (live + history)", bundled
with the P2 "CDR call-quality fields" item (the ROADMAP itself notes they
complement each other).

## 1. Problem

Operators can answer "is the fleet healthy?" (`/metrics` aggregates) and
"what happened on this one call?" (HEP → Homer), but not the middle
question: **"chart per-call quality over time, in my own dashboards."**
Meanwhile the bot server gets a thin `rtp_stats` (three RR-derived
fields), and the CDR — the record everyone already ingests — carries no
quality summary at all. Two documented gaps (`docs/OPERATIONS.md` Q5/Q8:
`first_audio_out_ms`, `barge_in_count`) have been parked since 0.9.x.

## 2. Current state (survey, 2026-07-10)

- **WS `rtp_stats`** (`[bridge].rtp_stats_interval_ms`, off by default):
  `jitter_ms`, `packet_loss_ratio`, `rtcp_rtt_ms` — all cached from
  forge's `RtcpReportReceived` (i.e. **remote-reported**: how the far end
  receives *our* stream). No local receive-side view at all.
- **forge-media already tracks the local side** — `JitterStats`
  (`packets_received/dropped/out_of_order/duplicate`) lives in the jitter
  buffer, and `forge-engine` builds `RtpQosReport`s (with an optional MOS
  field) for HEP on every RR — **but exposes no per-call stats event or
  snapshot API** to the embedding daemon. The HEP path is wired
  engine→emitter internally. Getting local RX stats into siphon-ai is
  upstream-gated (small PR).
- **CDR v3** has consent/park/hold/reconnect blocks but zero quality
  fields; `MediaTap` sees every barge-in `Clear`; the tap also sees the
  first outbound WS frame (for `first_audio_out_ms`) — both are
  daemon-side only, no upstream need.
- **Delivery infra is already built**: `siphon-ai-http` gives any sink
  HMAC signing + idempotency + durable spool (0.11.0); webhooks and CDR
  sinks both ride it. `examples/observability` has dashboards-as-code
  with a CI metric-name drift guard.

## 3. Sub-item 1 — Richer live stats + CDR quality summary (→ v0.30.0)

**Both directions on the wire, quality summary in the CDR.**

- **Upstream (forge-media PR)**: a periodic per-call
  `ForgeEvent::MediaStatsSnapshot` carrying the local receive-side
  counters forge already tracks (`packets_received`, `packets_dropped`,
  `packets_out_of_order`, `packets_duplicate`, locally-computed
  interarrival jitter). Event-push (like `RtcpReportReceived`), not a
  poll API — it matches the existing event plumbing and needs no new
  locking. Cadence set by the embedder when constructing the engine.
- **WS `rtp_stats` grows additive optional fields** (protocol stays v1;
  absent = unknown, exactly like `rtcp_rtt_ms` today). Existing fields
  keep their meaning (remote-reported / TX quality); new fields are
  explicitly RX-side: `rx_jitter_ms`, `rx_packets_received`,
  `rx_packets_lost`, `rx_packets_out_of_order`, `rx_packets_duplicate`,
  plus `mos_estimate` (see D5). Schema regenerated; both SDKs +
  PROTOCOL.md updated in the same PR (§7.1 checklist now enforces this).
  *(Implementation note: the note originally said `rx_packets_dropped`,
  mirroring forge's jitter-buffer naming — but the forwarding path
  measures sequence-gap transit loss, so the field shipped as
  `rx_packets_lost`, the RFC 3550 term. `rx_packets_duplicate` was added
  for forge/WS/CDR symmetry.)*
- **CDR `quality` block** (additive optional; **CDR_VERSION → 4**):
  `first_audio_out_ms` (bridge-connected → first WS binary frame),
  `barge_in_count` (tap `Clear` count), `avg/max_jitter_ms`,
  `avg/max_packet_loss_ratio`, `avg_rtcp_rtt_ms`, `rx_packets_*` totals,
  `mos_estimate_min/avg`. Aggregated in the `CallController` from the
  same snapshots — no second data path.

## 4. Sub-item 2 — Quality history + export (→ v0.31.0)

**Records operators can ingest, not a database we run.**

- **Per-call quality records**: one JSON object per call per
  `[quality]`-configured interval plus a final end-of-call summary —
  same shape as the CDR `quality` block plus `call_id`/`ts`/`seq`.
- **Sinks reuse the 0.11.0 delivery stack verbatim**: a JSONL file sink
  (like the CDR file sink) and/or a webhook sink (HMAC-signed, durable
  spool). No embedded DB, no query engine — operators already run
  TSDBs/Lokis; our job is a clean, signed, durable feed. "Queryable
  history" = their store, our export.
- **Live snapshot on the admin API**: `GET /admin/v1/calls/{id}/stats`
  (readonly role) returns the current tracker state for one call —
  the "what is this call doing *right now*" probe.
- **Dashboards**: extend `examples/observability` with a per-call
  quality dashboard fed by the webhook→(vector/promtail)→Loki/Influx
  path, documented end-to-end in `docs/OPERATIONS.md`.
- New metrics for the new paths (`siphon_ai_quality_records_total`,
  delivery counters come free from `siphon-ai-http`).

## 5. What this theme is NOT

- **No embedded database** (sqlite/duckdb) — new dep + retention +
  query-API surface for something operators' TSDBs do better.
- **No per-call Prometheus labels** (cardinality; the CLAUDE.md rule
  stands). History flows through records, not `/metrics`.
- **No AI/audio-content analysis** (speech quality scoring from PCM
  etc.) — MOS here is transport-math only.
- **No protocol version bump** — every wire change is additive optional.

## 6. Decisions (LOCKED 2026-07-13)

- **D1 — Local RX stats source**: new periodic
  `ForgeEvent::MediaStatsSnapshot` in forge-media (small upstream PR;
  counters already exist), cadence configured by the embedder.
  Alternative rejected: poll API (new locking surface); RR-only
  (leaves the RX side permanently blind).
- **D2 — Wire shape**: extend the existing `rtp_stats` event with
  additive optional `rx_*` + `mos_estimate` fields; keep existing three
  fields' remote/TX semantics. Alternative rejected: a second
  `media_stats` event (two events for one concern; consumers must join).
- **D3 — History**: JSONL file sink + signed/spooled webhook sink
  reusing `siphon-ai-http`, plus `GET /admin/v1/calls/{id}/stats` for
  live; no embedded store. Off by default via a new `[quality]` block.
- **D4 — CDR**: add the optional `quality` block, CDR_VERSION 3 → 4
  (additive-optional, but the version bump follows the 0.9.5 precedent
  for new blocks).
- **D5 — MOS**: compute a transport-only MOS-CQE estimate (simplified
  E-model from jitter/loss/RTT, the same math heplify-server applies to
  our HEP QoS chunks) — populated only when enough inputs exist, `null`
  otherwise. Alternative rejected: leave MOS to consumers (every
  consumer reimplements it differently; Homer-side and WS-side numbers
  would disagree).
- **D6 — Release slicing**: v0.30.0 = forge PR + richer `rtp_stats` +
  CDR quality block; v0.31.0 = `[quality]` records/sinks + admin
  endpoint + dashboards. Two releases, not three — sub-item 1 is one
  coherent data-path change, sub-item 2 is one delivery change.

## 7. Build order

1. **v0.30.0** — forge-media `MediaStatsSnapshot` PR → pin bump →
   `RtpStatsTracker` grows RX fields + MOS → additive `rtp_stats`
   fields (schema regen, SDKs, PROTOCOL.md) → CDR `quality` block
   (CDR_VERSION 4) + `first_audio_out_ms`/`barge_in_count`. Verify:
   SIPp call shows populated RX fields + CDR quality block; testkit
   corpus still green; OPERATIONS.md Q5/Q8 gaps closed.
2. **v0.31.0** — `[quality]` config block + record emitter (off the
   audio path, worker task) + file/webhook sinks via `siphon-ai-http` +
   `GET /admin/v1/calls/{id}/stats` + observability example +
   OPERATIONS.md ingestion guide. Verify: records flow + signature
   validates; spool survives restart; dashboard renders; theme
   retrospective here.

## 8. Retrospective (2026-07-14, theme complete)

**Shipped as planned, two releases:**

- **v0.30.0** (#279 + #280): forge-media#81 (`MediaStatsSnapshot` +
  `RxStreamStats`), `rtp_stats` `rx_*` + `mos_estimate` (protocol stayed
  v1), CDR v4 `quality` block, OPERATIONS Q5/Q8 closed.
- **v0.31.0** (single feature PR + release): `crates/quality`
  (records = the CDR block flattened, JSONL + HMAC/spool webhook over
  `siphon-ai-http`), `[quality]` config, per-call record task,
  `GET /admin/v1/calls/{id}/stats` via a `core::quality_live` RAII
  registry, Vector→Loki reference pipeline + Grafana history dashboard.

**Deviations from the locked design, all small:**

- D2 naming: `rx_packets_dropped` → `rx_packets_lost` (the forwarding
  path measures sequence-gap transit loss, not jitter-buffer drops);
  `rx_packets_duplicate` added for forge/WS/CDR symmetry.
- The forge counters weren't actually on the media path yet — the
  jitter buffer that §2 pointed at is unused there. forge-media#81
  added RFC 3550-style tracking to the forwarding engine itself.
- The bridge-ready oneshot became an `Instant`-carrying watch: three
  consumers (CDR build, record task, live endpoint) need the
  first-audio epoch, not one.

**What worked well:** one shape (`siphon_ai_cdr::QualityInfo`) feeding
three surfaces (CDR / records / live endpoint) killed every drift
question at review time; the audit crate was a near-verbatim template
for the sink stack (a day saved); the tap's watch channel meant zero
new state on the audio path.

**Worth remembering:** the local SIPp suite silently fails 8/34 without
the echo server on :8765 (cost an hour of triage before a main-branch
baseline proved it environmental); forge's `--features ha` restore path
doesn't compile in default local checks — CI caught the missing
`rx_stream` initializer.
