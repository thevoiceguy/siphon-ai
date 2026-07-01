# Audit-event stream (0.20.0)

SiphonAI can emit a **signed, tamper-evident audit trail** of admin and
security decisions for ingestion by a SIEM. It answers two questions an
incident review needs:

- **Who did what** on the privileged `[admin]` API surface.
- **What the daemon refused** on the SIP surface — failed authentication,
  admission shedding, STIR/SHAKEN policy rejection — plus config and TLS
  cert reloads.

It is **off by default** and distinct from the two other out-of-band
streams: `[webhooks]` (lifecycle events for ops automation) and `[cdr]`
(call-detail records for billing). Configuration lives under `[audit]` —
see [`CONFIG.md`](CONFIG.md#audit--signed-audit-event-stream-0200).

## Transports

Enable either or both:

- **File** (`[audit.file]`) — an append-only JSONL file, one event per
  line. The on-box trail: it keeps recording even if the SIEM or network
  is down, and the usual ingestion path is a log shipper (Vector /
  Filebeat / Fluent Bit) tailing it. For tamper-*resistance* beyond an
  append-only file, ship it off-box promptly and/or point `path` at a
  WORM/append-only mount.
- **Webhook** (`[audit.webhook]`) — an HTTP POST per event to a SIEM
  collector, reusing the same delivery transport as `[webhooks]`/`[cdr]`.
  With a `secret` set, every request carries an HMAC-SHA256
  `X-SiphonAI-Signature: t=<unix>,v1=<hex>` (signed over `<unix>.<body>`)
  — **this is what makes the stream tamper-evident**: a SIEM that verifies
  the signature detects any in-flight modification and, via the
  timestamp, replay. Deliveries also carry `X-SiphonAI-Event-Id` for
  idempotent dedupe, retry transient failures with backoff, and — with a
  `spool_dir` — survive restarts. See
  [`DEPLOY.md`](DEPLOY.md) → *Webhook delivery: signing, idempotency,
  durability* (identical mechanism, `sink="audit"` on the metrics).

## Signal, not noise

The stream deliberately records the **anomalies** a security team acts
on, not routine call volume:

- `invite_rejected` records admission **`rate_limited`** (503) and the
  **`no_trunk`** (403) / **`draining`** (503) refusals — but **not** the
  per-packet silent flood-drop. That drop path is the DoS-shedding fast
  path; auditing it would amplify the very attack it defends against. The
  *onset* of shedding is captured by the `rate_limited` events.
- `sip_auth` records **`failed`** (bad credential) and **`stale`**
  (possibly-replayed nonce) — but **not** the normal first-leg
  `challenged` 401 (fires on every authenticated call, before any
  credential) or a successful `ok` (both track call volume, not security).

Use the `events` allowlist to narrow further (e.g. only `admin_request`
and `sip_auth`).

## Event schema

Each event is a JSON object with a `type` discriminator, a `version`
(schema version, currently **1** — additive changes keep it, a
breaking change bumps it), and a `timestamp` (UTC, RFC 3339). Optional
fields are omitted when absent.

### `admin_request`

A request to the authenticated `[admin]` listener.

| Field           | Notes |
|-----------------|-------|
| `peer`          | Client socket address. |
| `actor`         | Token *name* that authenticated (never the secret); omitted when unauthenticated. |
| `role`          | `readonly` / `operator` / `admin`; omitted when unauthenticated. |
| `method`        | HTTP method. |
| `endpoint`      | Matched route template, e.g. `/admin/v1/calls/:id` (bounded cardinality). |
| `status`        | HTTP status returned. |
| `result`        | `ok` \| `unauthenticated` \| `forbidden` \| `not_found`. |
| `required_role` | For `forbidden`: the role the endpoint required. |

> Note: on a `forbidden` (RBAC denial) the caller authenticated but the
> token *name* isn't surfaced at that layer, so `actor` is absent while
> `role` carries what the token had.

### `sip_auth`

Outcome of inbound SIP digest authentication (`[sip.auth]`). Only the
anomalous outcomes are emitted (see *Signal, not noise*).

| Field             | Notes |
|-------------------|-------|
| `peer`            | Source of the INVITE. |
| `register_source` | The credential source (trunk name) the challenge is scoped to. |
| `result`          | `failed` \| `stale`. |

### `invite_rejected`

An inbound INVITE refused before routing.

| Field    | Notes |
|----------|-------|
| `peer`   | Source of the INVITE. |
| `result` | `rate_limited` (admission 503) \| `no_trunk` (allowlist 403) \| `draining` (shutdown 503). |

### `attestation_rejected`

An inbound INVITE refused on STIR/SHAKEN grounds (`[security]`).

| Field         | Notes |
|---------------|-------|
| `from_tn`     | Originating TN from the PASSporT, when available (not plumbed to this layer today → usually absent). |
| `required`    | Minimum attestation the call failed to meet (`A`/`B`/`C`), or `identity_required` when an Identity header was mandatory and missing. |
| `attestation` | Attestation the call presented, when any. |
| `code`        | SIP response code used (403 / 428 / 488 / 606). |
| `reason`      | Reason phrase. |

### `config_reload`

Outcome of a `SIGHUP` configuration reload. (A no-op reload — file
unchanged — is deliberately **not** emitted.)

| Field              | Notes |
|--------------------|-------|
| `result`           | `applied` \| `failed`. |
| `restart_required` | Sections that changed but need a full restart to take effect (present only when non-empty). |
| `detail`           | Failure detail for `failed`. |

### `cert_reload`

A TLS certificate hot-reload on `SIGHUP`, for the admin or SIP listener.

| Field       | Notes |
|-------------|-------|
| `component` | `admin_tls` \| `sip_tls`. |
| `cert_path` | Certificate file path. |
| `result`    | `ok` \| `failed`. |
| `detail`    | Failure detail for `failed`. |

## Hot reload

Changes to an **already-enabled** `[audit]` block are hot-applied on
`SIGHUP` (retarget the webhook, add the file sink, change the allowlist,
or disable the stream). *Enabling* audit from off is **restart-required**
— the process-global emission facade is installed once at startup. A
durable `spool_dir` makes webhook delivery changes restart-required too
(its drain worker can't be hot-swapped), same as `[webhooks]`/`[cdr]`.

## Delivery health

The webhook sink shares the `siphon_ai_webhook_*` delivery metrics with
the other HTTP sinks, tagged `sink="audit"`:

- `siphon_ai_webhook_deliveries_total{sink="audit",result}` —
  `delivered` / `rejected` / `spooled` / `dropped`.
- `siphon_ai_webhook_delivery_attempts_total{sink="audit"}`.
- `siphon_ai_webhook_delivery_seconds{sink="audit"}`.
- `siphon_ai_webhook_spool_depth{sink="audit"}`.

Audit emission is **best-effort and off the call path** (CLAUDE.md §4.7):
a slow or unreachable SIEM never blocks an admin request or a SIP
transaction — the event is spooled or dropped, and the call proceeds.
