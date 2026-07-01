# SiphonAI observability example — Prometheus + Grafana

Shipped, ready-to-run **dashboards and alerts as code** for a SiphonAI fleet.
SiphonAI exposes 40-plus `siphon_ai_*` Prometheus metrics on its
`[observability]` listener (`/metrics`, default `127.0.0.1:9091`); this
directory turns them into recording rules, alerts, and two Grafana dashboards
you can drop into an existing stack or run standalone.

For per-call SIP flow and per-stream RTP quality, pair this with
[`../homer-stack/`](../homer-stack/) (HEP → Homer). Rule of thumb: **Prometheus
+ Grafana here for rates, ratios, and latency percentiles; Homer for the
individual call.** The `docs/OPERATIONS.md` "ten questions" runbook says which
to open for a given symptom.

## What's here

```
prometheus.yml                 reference scrape config (+ loads the rules)
rules/recording.yml            pre-aggregated rates / ratios / latency percentiles
rules/alerting.yml             starting-point alerts (availability, calls, latency,
                               delivery, security, shutdown)
grafana/
  provisioning/                datasource (uid siphonai-prom) + dashboard provider
  dashboards/
    siphon-ai-fleet.json       Fleet Overview — volume, outcomes, routes,
                               registrations, delivery health, drain
    siphon-ai-call-quality.json  Call Quality — WS/SDP/duration latency, RTP RTT
                               & loss, conference mixer health
compose.yaml                   Prometheus + auto-provisioned Grafana
```

## Run it

```sh
docker compose -f examples/observability/compose.yaml up
```

- **Grafana** → http://127.0.0.1:3000 (`admin` / `admin`) — both dashboards
  are pre-loaded under the `siphon-ai` tag.
- **Prometheus** → http://127.0.0.1:9090 — check *Status → Rules* and *Alerts*.

By default Prometheus scrapes a daemon at `siphon-ai:9091` (mapped to the
host gateway, so a daemon running on the host is reachable). Point it at your
own daemon(s) by editing [`prometheus.yml`](prometheus.yml).

Reference-only: ports are published on loopback, credentials are defaults,
and there's no storage persistence. Harden before any shared use.

## Tuning

Thresholds and `for:` windows in `rules/alerting.yml` are conservative
defaults — set them to your SLOs and traffic shape. The recording rules use
`[5m]` windows; widen for quieter fleets. Wire your own Alertmanager under
`prometheus.yml`'s `alerting:` block (intentionally omitted here).

## Kept honest by CI

`scripts/check-observability-metrics.py` (run in CI) asserts every
`siphon_ai_*` metric referenced in these rules and dashboards is a metric the
daemon actually emits, and `promtool check config` validates the PromQL — so
a metric rename can't ship silently-broken artifacts. If you add a panel or
rule for a new metric, both run locally too:

```sh
python3 scripts/check-observability-metrics.py
docker run --rm -v "$PWD/examples/observability":/work -w /work \
  --entrypoint promtool prom/prometheus:latest check config prometheus.yml
```
