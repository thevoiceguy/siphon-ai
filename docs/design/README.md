# Design notes & historical dev plans

Internal planning artifacts — **write-once design records**, not operator
documentation. They capture the *why* and the decision history behind each
feature theme. For how to deploy, configure, and operate SiphonAI, see the
top-level [`docs/`](../) (PROTOCOL, CONFIG, DEPLOY, the feature guides, etc.).
The canonical "what & why" overview, [`docs/DEV_PLAN.md`](../DEV_PLAN.md),
stays at the top level.

## Design notes

One per feature theme — the locked decisions and the implementation sketch:

- [DESIGN_OBSERVABILITY.md](DESIGN_OBSERVABILITY.md) — observability completeness: dashboards/alerts as code + OTLP traces (v0.21.0+, decisions locked)
- [DESIGN_SECURITY_HARDENING.md](DESIGN_SECURITY_HARDENING.md) — security & abuse hardening: admin TLS + secret sources, inbound digest auth + admission, signed audit stream (v0.18–0.20)
- [DESIGN_ADMIN_AUTH.md](DESIGN_ADMIN_AUTH.md) — native admin auth + RBAC (v0.10.0)
- [DESIGN_WEBHOOK_DURABILITY.md](DESIGN_WEBHOOK_DURABILITY.md) — webhook/CDR signing, idempotency, durable spool (v0.11.0)
- [DESIGN_CONFIG_CLI.md](DESIGN_CONFIG_CLI.md) — config CLI (`check`/`print-config`/`route-test`) + SIGHUP reload (v0.12.x)
- [DESIGN_DELAYED_OFFER.md](DESIGN_DELAYED_OFFER.md) — offerless/delayed-offer INVITEs (v0.9.0)
- [DESIGN_OPUS.md](DESIGN_OPUS.md) — Opus codec at a 16 kHz bridge rate (v0.8.0)
- [DESIGN_HOLD.md](DESIGN_HOLD.md) — bot-initiated hold/resume (v0.7.2)
- [DESIGN_WS_RECONNECT.md](DESIGN_WS_RECONNECT.md) — mid-call WS reconnect (v0.7.3)
- [DESIGN_0.7.0_PARK.md](DESIGN_0.7.0_PARK.md) — media-only call park (v0.7.0)

## Versioned dev plans

The per-release plans that drove each milestone:

- [DEV_PLAN_0.2.0.md](DEV_PLAN_0.2.0.md)
- [DEV_PLAN_0.3.0.md](DEV_PLAN_0.3.0.md)
- [DEV_PLAN_0.4.0.md](DEV_PLAN_0.4.0.md)
- [DEV_PLAN_0.4.1.md](DEV_PLAN_0.4.1.md)
- [DEV_PLAN_0.5.0.md](DEV_PLAN_0.5.0.md)
- [DEV_PLAN_0.6.0.md](DEV_PLAN_0.6.0.md)
- [DEV_PLAN_0.6.1.md](DEV_PLAN_0.6.1.md)
- [DEV_PLAN_0.7.0.md](DEV_PLAN_0.7.0.md)

## Spikes

- [SPIKE_MEDIA_TAP.md](SPIKE_MEDIA_TAP.md) — the Week-1 media-tap feasibility spike
