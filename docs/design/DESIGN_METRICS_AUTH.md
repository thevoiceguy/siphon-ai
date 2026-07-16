# Design note — optional `/metrics` bearer auth

> **Status: APPROVED — decisions LOCKED (§6, all per recommendation,
> 2026-07-15).** The smallest theme on
> the board; the note exists to pin the semantics, not to explore.
> Target: **0.35.0**. No WS-protocol, CDR, or webhook changes.

Adds an **optional bearer token on `GET /metrics`** — and only there —
for deployments that expose the observability port more widely than
loopback. This is recon-hardening, not a PII fix: `/metrics` carries
only aggregate counters plus operator-chosen route/register names (the
ROADMAP item records that audit), but fleet shape, carrier names, and
call volumes are still reconnaissance a stranger shouldn't get for
free. Off by default — unset means today's open endpoint, unchanged.

---

## 1. Config

```toml
[observability]
enabled = true
http_listen = "0.0.0.0:9090"
metrics_token = "${file:/etc/siphon-ai/metrics.token}"   # NEW, optional
```

- `metrics_token` — the expected bearer token. Optional; **unset =
  open** (the default, today's behavior). Empty-after-expansion is a
  load error (an empty gate is a config mistake, not a policy).
  `${VAR}` / `${file:…}` / `${cred:…}` resolution applies as
  everywhere (0.18.0) — never inline the secret.
- `[observability]` stays **restart-required** on SIGHUP, like the
  rest of the block.

## 2. Behavior

- With the token set, `GET /metrics` requires
  `Authorization: Bearer <token>`. Comparison is **SHA-256 +
  constant-time**, reusing the admin listener's scheme (`auth.rs`,
  0.10.0) — no token material or hash ever logged.
- Missing/malformed/wrong credentials → **`401`** with
  `WWW-Authenticate: Bearer` (so Prometheus's target page and `curl`
  are self-explanatory). No 403 tier — there are no roles here.
- **`/health` and `/ready` stay open** unconditionally (the ROADMAP
  contract): Kubernetes probes and load balancers must not need
  secrets. Everything else on the listener remains a 404, gate or no
  gate.
- Prometheus side: `authorization: { credentials_file: … }` in the
  scrape config — documented in `docs/DEPLOY.md` with a snippet, and
  noted in `examples/observability/prometheus.yml`.

## 3. Observability of the gate

`siphon_ai_metrics_requests_total{result = ok | unauthenticated}`,
counted only when the gate is configured (an open endpoint counts
nothing — zero-noise default). A sudden `unauthenticated` ramp is a
misconfigured scraper or a prober; either is worth seeing. A
rate-limited `warn!` (first rejection per minute) accompanies it.

## 4. Testing

- **http.rs unit/integration** (the existing `ObservabilityServer`
  test harness): 401 without a header, 401 with the wrong token, 200
  with the right one, `/health` + `/ready` open with the gate on,
  everything open with the gate off.
- **Config load**: unset → `None`; set → gate configured;
  empty-after-expansion → load error.
- The SIPp harness's `curl …/metrics` asserts run against configs
  without the token — untouched.

## 5. Docs

`docs/CONFIG.md` (`[observability]` table), `docs/DEPLOY.md` (metric
row + the Prometheus auth snippet in the security notes),
`examples/observability/prometheus.yml` comment. ROADMAP P2 item
marked delivered in the release PR.

---

## 6. Decisions to lock

1. **Key: `[observability].metrics_token`** (vs a nested
   `[observability.auth]` block). **Recommend the flat key** — one
   knob doesn't need a block.
2. **`401` + `WWW-Authenticate: Bearer`**, no roles, no 403 tier.
   **Recommend as stated.**
3. **`/health` + `/ready` unconditionally open.** **Recommend as
   stated** (the ROADMAP contract; probes must not need secrets).
4. **Gate-scoped counter + rate-limited warn** (§3), silent when the
   gate is off. **Recommend yes** — near-zero cost, catches the
   broken-scraper case immediately.
5. **Restart-required** (no SIGHUP hot-reload of the token).
   **Recommend yes** — matches the block; token rotation is a rolling
   restart, which 0.17.0 made zero-drop anyway.
6. **Version 0.35.0, two PRs** (feature, release) — the standing
   cadence. **Recommend as stated.**
