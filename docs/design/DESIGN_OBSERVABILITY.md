# Design: observability completeness (P1 theme)

> **Status: DECISIONS LOCKED (2026-07-01) — §5.** Same design-first cadence
> as the security-hardening theme (→ v0.18–0.20) and every release before it:
> design note → locked decisions → chunked PRs → tag-after-merge. The build
> follows §6; deviations get noted back here.

Theme: **P1 "Observability completeness" from `docs/ROADMAP.md`**, the top open
theme now that the security & abuse hardening theme is complete (audit stream
→ v0.20.0). The roadmap frames it as: *"the metrics/logs/HEP primitives are
rich, but there are no shipped consumer artifacts and no distributed
tracing."* Two sub-items:

1. **Dashboards & alerts as code** — shipped Grafana dashboards + Prometheus
   recording/alerting rules + the `docs/OPERATIONS.md` "ten questions"
   runbook made concrete.
2. **OpenTelemetry / OTLP traces** — today tracing is structured logs + HEP
   correlation only; OTLP spans would let an operator trace a call across the
   daemon **and** the developer's WS server in one view.

The headline finding from the code survey: **the primitives are already
there and clean.** 36 `siphon_ai_*` metrics with bounded labels and explicit
histogram buckets, a Prometheus exporter on `/metrics`, 15 `#[instrument]`
spans with a per-call root, and HEP correlation by SIP Call-ID. The gap is
**consumer artifacts** (sub-item 1, pure docs/examples — no code) and a
**tracing export layer** (sub-item 2, additive, off by default). Nothing here
needs a `forge-media` or `siphon-rs` change.

---

## 1. The gaps today

### 1.1 Dashboards & alerts as code

- **36 metrics, zero consumer artifacts.** `crates/telemetry/src/metrics.rs`
  defines 20 counters, 7 gauges, 7 histograms (all `siphon_ai_`-prefixed,
  bounded labels, buckets set in `prometheus_builder()`), exported via
  `metrics-exporter-prometheus` on `[observability].http_listen` `/metrics`
  (`crates/telemetry/src/http.rs:171-177`). But the repo ships **no**
  Grafana dashboard JSON, **no** `*.rules.yml` recording/alerting rules, and
  **no** `prometheus.yml` scrape config. An operator gets a raw `/metrics`
  page and a blank Grafana.
- **`homer-stack` has no metrics layer.** `examples/homer-stack/compose.yaml`
  stands up Postgres + heplify-server + Homer webapp (HEP → SIP/RTP flow +
  QoS), and its README explicitly says Prometheus/Grafana are "out of scope
  for this example." `docker/compose.yaml` exposes `/metrics` on 9091 but
  wires no scraper.
- **The runbook is abstract.** `docs/OPERATIONS.md` answers the §11.8 "ten
  questions" (why this route? why this codec? why did it drop? where was the
  latency? …) in principle — pointing at log lines, spans, HEP, and the three
  latency histograms — but has **no worked PromQL, no dashboard panel
  pointers**. It tells you *what's answerable*, not *the query to run*.
- **No metrics reference.** `docs/DEPLOY.md` documents the `[observability]`
  port and a handful of metrics inline, but there's no single table of every
  metric + its labels — the thing you'd read to author an alert. CLAUDE.md
  §7.4 step 3 ("document it in `docs/DEPLOY.md`") has drifted.

### 1.2 OTLP traces

- **Tracing is logs + HEP only.** `bins/siphon-ai/src/main.rs:init_tracing`
  builds `Registry → reload(EnvFilter) → fmt::layer()` (with the
  `LogFilterHandle` runtime-reload seam, `crates/telemetry/src/log_filter.rs`).
  `crates/telemetry/src/lib.rs` even flags OTLP as "NOT here yet." There is no
  span exporter — spans exist only as structured-log context.
- **Spans are per-call but fragmented across tasks.** 15 `#[instrument]`
  spans; the per-call root is `CallController::run`
  (`fields(call_id = %self.cfg.call_id)`), with `on_invite`, the media-setup
  spans, and `connect_and_run` (the WS bridge) around it. But
  `connect_and_run` runs on a **separately spawned task**, so it's a *sibling*
  of the controller span, not a child — a call is several disconnected span
  trees, not one trace. Making a call a single trace means `.instrument()`-ing
  the spawned tasks with the root span (or its context).
- **Correlation stops at the daemon.** HEP stitches SIP messages, RTCP/QoS,
  logs, VERSTAT, and CDR by **SIP Call-ID**
  (`crates/telemetry/src/hep.rs`) into Homer's call view. The WS server gets
  the **bridge** `call_id` via the `X-Siphon-Call-Id` upgrade header
  (`docs/PROTOCOL.md`) for its own log correlation — but there's no shared
  trace context, so the developer can't see "daemon span → my WS-server span"
  in one waterfall.
- **No OTLP deps in the tree.** `opentelemetry`, `opentelemetry-otlp`,
  `tracing-opentelemetry`, `tonic`, `prost` are all absent (CLAUDE.md §4.2:
  small dep tree on purpose — this theme's one approved exception, §5).

---

## 2. Sub-item 1 — Dashboards & alerts as code (→ v0.21.0)

**Pure artifacts + docs. No code change, no new deps, no protocol touch.** The
lowest-risk, fastest-to-value release; ships first (decision 1).

### 2.1 Deliverables

- **`examples/observability/`** — a self-contained, provisioned stack:
  - `prometheus.yml` — reference scrape config for the daemon `/metrics`
    (+ a note on remote-write).
  - `rules/recording.yml` — aggregates the raw metrics into the shapes
    dashboards/alerts want: per-route call rate, INVITE accept/reject ratio,
    call-duration and WS-connect and SDP-negotiate latency percentiles
    (`histogram_quantile` over `_bucket`), RTP-RTT percentiles, webhook
    delivery success ratio, spool depth.
  - `rules/alerting.yml` — the pages an operator actually wants: high INVITE
    reject/drop rate, WS-connect p99 over threshold, a `[[register]]` gone
    `failed`, webhook/CDR/audit spool backing up, delivery-failure ratio,
    `drain_forced` > 0, RTP-RTT p95 high, and a "no INVITEs in N minutes"
    dead-air alert. Each alert annotated with a link to the relevant
    `OPERATIONS.md` question.
  - `grafana/` — provisioned datasource + dashboard JSON. Two dashboards:
    **Fleet overview** (call volume/outcome/duration, registration grid,
    active/draining, webhook+spool health) and **Call quality** (WS-connect &
    SDP-negotiate latency, RTP RTT, conference tick lag, room frame drops).
  - `compose.yaml` — Prometheus + Grafana (auto-provisioned with the above),
    scraping a daemon. Composable with `homer-stack` for the full picture
    (Prometheus/Grafana for rates+latency, Homer for per-call SIP/RTP flow).
- **`docs/DEPLOY.md` metrics reference** — one table of every `siphon_ai_*`
  metric, its type, and its labels (closes the CLAUDE.md §7.4 drift). This
  becomes the canonical list the rules/dashboards are built from.
- **`docs/OPERATIONS.md` made concrete** — each of the ten questions gets the
  worked artifact: the PromQL / dashboard panel / Homer view to open, and
  which alert (if any) fires for it. Media-quality depth (per-stream loss &
  jitter) stays pointed at **Homer/HEP** — those live in `forge_rtcp_*` /
  RTP-QoS chunks, not the `siphon_ai_` Prometheus namespace, and that
  boundary is stated explicitly rather than papered over.

### 2.2 Anti-drift

Dashboards and rules reference metric names as strings; a rename in
`metrics.rs` would silently break them. Ship a small **CI check** (a script
under `scripts/`, run in the existing lint workflow) that extracts every
`siphon_ai_*` name referenced in the rules/dashboard JSON and asserts it
exists in `metrics.rs` — same spirit as the `version consistency` gate.

---

## 3. Sub-item 2 — OTLP traces (→ v0.22.0 daemon-side, → v0.23.0 WS propagation)

**Additive, off by default, best-effort (never blocks the call path, §4.7).**
Split into a daemon-internal release and a WS-propagation release so the
protocol touch is isolated and independently reviewable.

### 3.1 v0.22.0 — daemon-side OTLP export

- **Deps (approved, decision 2):** `opentelemetry`, `opentelemetry-otlp`,
  `tracing-opentelemetry`, and the gRPC transport (`tonic`/`prost`). Confined
  to `siphon-ai-telemetry` + the daemon binary.
- **Config `[observability.otlp]`** — `enabled` (default `false`), `endpoint`
  (OTLP gRPC, default `http://localhost:4317`), `protocol` (grpc; http/proto
  as a possible later knob), `sample_ratio`, and resource attributes
  (`service.name` etc.). Validated at load; enabling with a bad endpoint
  fails loud like every other sink.
- **Export layer** — in `init_tracing`, add a `tracing_opentelemetry::layer()`
  over a batch-exporting `TracerProvider` alongside the existing reload +
  fmt layers (the layered subscriber already supports this cleanly). The
  batch exporter runs on its own task with a **graceful flush on shutdown**,
  mirroring the HEP worker's lifecycle — a slow/absent collector never stalls
  a span or the call path.
- **One trace per call** — set the `CallController::run` span as the trace
  root and stamp OTel-semantic-convention attributes on it (`siphon.call_id`,
  `sip.call_id`, `route`, direction). `.instrument()` the spawned WS-bridge
  and media tasks with the root context so they become child spans instead of
  siblings — the one real (small) refactor in this theme.
- **Metrics-over-OTLP is out of scope** — Prometheus scrape stays the metrics
  path (sub-item 1). OTLP is traces-only here; OTLP metrics export can be a
  later knob if demand appears.

### 3.2 v0.23.0 — W3C trace propagation to the WS server (decision 3: yes)

- **Backwards-compatible, protocol stays v1.** Inject a W3C
  `traceparent` (+ `tracestate`) header on the **WS upgrade request** — the
  spec already documents upgrade headers as inspectable and that SiphonAI sets
  custom ones (`X-Siphon-Call-Id`), so this is "one more header," zero-break.
  Optionally also expose the context as an additive `trace_context` field on
  the JSON `start` message (servers that don't read it ignore it, same as
  `reconnected` / `srtp` / `verstat`).
- **Docs + examples** — `PROTOCOL.md` gains one line in the headers table and
  a `start`-field note (explicitly called out as additive, version unchanged,
  per CLAUDE.md §4.2). The reference WS servers (`examples/echo-ws-server-*`,
  `openai-realtime-bridge-py`) show extracting `traceparent` and continuing
  the trace, so a developer sees daemon + their server in one waterfall — the
  payoff of the whole OTLP arc.

---

## 4. What this theme is NOT

- **Not** a metrics-schema change — no new `siphon_ai_*` metrics are required
  for sub-item 1 (it consumes what exists). If authoring the runbook surfaces
  a genuinely missing signal (e.g. the `OPERATIONS.md` §11.8 follow-ups —
  `sip_terminator` on the end log, `first_audio_out_ms` / `barge_in_count` on
  the CDR), that's a small opportunistic add, noted per-PR, not the theme's
  spine.
- **Not** an OTLP metrics pipeline (Prometheus scrape stays canonical).
- **Not** a HEP replacement — HEP/Homer remains the SIP-flow + per-stream
  media-quality view; OTLP is the cross-service **latency/causality** view.
  They complement (SIP Call-ID stitches HEP; trace_id stitches OTLP; the
  bridge `call_id` is carried as an attribute/header in both so an operator
  can hop between them).

---

## 5. Decisions (LOCKED 2026-07-01)

1. **Sequence: dashboards first, then OTLP.** v0.21.0 (dashboards/rules/
   runbook — no code) ships as a self-contained release; OTLP (v0.22 daemon,
   v0.23 WS propagation) follows. Incremental value before the dep/protocol
   commitment.
2. **OTLP deps approved.** Add `opentelemetry` + `opentelemetry-otlp` +
   `tracing-opentelemetry` + `tonic`/`prost` (gRPC transport) — the theme's
   one sanctioned exception to the small-dep-tree rule. Runtime-optional,
   off by default.
3. **Propagate W3C trace context to the WS server.** `traceparent`/`tracestate`
   on the WS upgrade (+ optional additive `start` field). Backwards-
   compatible; **protocol stays v1**.
4. **Off by default, fail-loud config, best-effort export.** OTLP mirrors HEP:
   never blocks the call path, graceful flush on shutdown, unreachable
   collector is not fatal. Dashboards/rules are inert artifacts (no runtime
   effect).
5. **Anti-drift CI check** for metric names referenced in shipped rules/
   dashboards (§2.2).

---

## 6. Build order

| Release | Sub-item | Shape |
|---|---|---|
| **v0.21.0** | Dashboards & alerts as code | `examples/observability/` (Prometheus scrape + recording + alerting rules + provisioned Grafana + compose), `DEPLOY.md` metrics reference, `OPERATIONS.md` worked queries, anti-drift CI check. No code, no deps. |
| **v0.22.0** | OTLP traces (daemon) | `[observability.otlp]` config, OTLP export layer + batch worker + graceful flush, one-trace-per-call (root span attrs + `.instrument()` the spawned tasks). New deps land here. Off by default. |
| **v0.23.0** | WS trace propagation | `traceparent`/`tracestate` on WS upgrade + optional `start` field; `PROTOCOL.md` + reference-server updates. Protocol stays v1. |

Each release: design-note-consistent, chunked PRs, `RELEASING.md` tag-after-
merge, verify published artifacts. Deviations noted back here.
