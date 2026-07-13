# Design: Per-call quality telemetry (live + history)

**Status: LOCKED (2026-07-13) — decisions in §6 confirmed; implementing per §7.**
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
  `rx_packets_dropped`, `rx_packets_out_of_order`, plus `mos_estimate`
  (see D5). Schema regenerated; both SDKs + PROTOCOL.md updated in the
  same PR (§7.1 checklist now enforces this).
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
