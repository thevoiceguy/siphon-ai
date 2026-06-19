# Design note — native admin authentication + RBAC

> **Status: DRAFT — gating decisions LOCKED (2026-06-19).** Same
> design-first pass we used for park / hold / reconnect / Opus / delayed
> offer. This is the biggest remaining product gap: the `/admin/*`
> surface can hang up calls, **originate billable outbound calls**,
> manage conferences, retrieve parked calls, and change log filters — and
> today it is **unauthenticated** (the code/docs rely on private bind or
> a reverse proxy: `admin.rs` threat-model note, `DEPLOY.md` §security).
> The build follows this once §6 is fully locked; deviations get noted
> back here.

Adds **daemon-native authentication + role-based authorization** to the
admin API, on its **own listener**, separate from the open metrics/health
listener.

---

## 1. The gap

Every `/admin/*` endpoint sits on the **same HTTP listener** as
`/metrics` / `/health` / `/ready` (`[observability].http_listen`), and
the daemon does **not** authenticate any of them. The current threat
model (`admin.rs`) is "bind on a trusted address; front with an
authenticating reverse proxy." That's a real operational burden and a
foot-gun: anything that can reach the port can:

| Endpoint | Power |
|---|---|
| `POST /admin/v1/calls` | **originate billable outbound calls** |
| `POST /admin/calls/:id/hangup` | drop any live call |
| `POST /admin/v1/calls/:id/retrieve` | pick up a parked call onto a chosen WS |
| `POST/DELETE /admin/v1/conferences/*` | create/destroy rooms, move calls |
| `PUT /admin/log` | change log verbosity at runtime |

`POST /admin/v1/calls` even documents "no built-in auth — the cap + rate
limit are the native guardrails." That's not enough for a billable
action.

---

## 2. Shape (locked decisions §6)

**Bearer tokens, fixed roles, a separate listener.**

```
                       ┌─ [observability].http_listen ─┐   (open, unauthenticated)
                       │   /health  /ready  /metrics    │ ← Prometheus scrape, k8s probes
   operator / scripts ─┤                                │
                       │                                 │
                       └─ [admin].listen ───────────────┘   (Bearer token required + RBAC)
                           /admin/*   (401 / 403 / 2xx)
```

- **`/admin/*` moves to a dedicated `[admin]` listener.** `/metrics`,
  `/health`, `/ready` stay on `[observability].http_listen`,
  unauthenticated, so existing scrapers and probes are untouched. The
  observability listener **no longer serves `/admin/*`** (see §5,
  back-compat).
- **Bearer tokens** (`Authorization: Bearer <token>`), defined in config,
  each carrying one **role**. Compared in **constant time** against the
  SHA-256 of the configured token (we store the hash, never the plaintext,
  in memory after load).
- **Fixed roles:** `readonly` ⊂ `operator` ⊂ `admin`.

---

## 3. Roles → endpoints

Three roles, nested (each includes the lower one's rights). Every
endpoint declares a **minimum role**; the middleware rejects a token
whose role is below it with `403`.

| Role | Grants |
|---|---|
| `readonly` | all `GET` / list: `/admin/calls`, `/admin/registrations`, `/admin/log` (read), `/admin/v1/conferences`, `/admin/v1/parked` |
| `operator` | readonly **+** live-call control: `hangup`, `park`, `retrieve`, conference create/end + participant add/remove |
| `admin` | operator **+** the dangerous/billable + config: `POST /admin/v1/calls` (originate), `PUT /admin/log`, `POST /admin/hep/test` |

(Originate and log-filter changes are `admin`-only on purpose — they are
the billable and the observability-blinding actions.)

---

## 4. Auth + audit mechanics

- **Token check.** Extract `Authorization: Bearer <t>`; if absent →
  `401` with `WWW-Authenticate: Bearer`. Hash `t` (SHA-256), constant-time
  compare against each configured token hash. No match → `401`. Match →
  resolve the token's role.
- **Authorization.** Look up the endpoint's minimum role; if the token's
  role is below it → `403`. Else dispatch as today.
- **Constant-time + no oracle.** Same `401` for "no token" and "bad
  token" beyond the `WWW-Authenticate` hint; never echo the token; the
  hash compare is constant-time to avoid timing leaks.
- **Audit.** Every authenticated admin action emits a structured audit
  **log** (`actor` = token *name*, never the secret; `action`; `target`;
  `result`; `peer`) at `info`, plus a metric
  `siphon_ai_admin_requests_total{endpoint,role,result}` with bounded
  labels. A **signed audit webhook** is a follow-up that lands with the
  P0 webhook-durability theme (it reuses that theme's HMAC signer rather
  than inventing a second one here).

---

## 5. Config + validation

```toml
[admin]
listen = "127.0.0.1:9092"        # dedicated admin bind (required to expose /admin/*)

[[admin.token]]
name  = "ops-dashboard"          # label for audit logs; not a secret
token = "${ADMIN_TOKEN_OPS}"     # env-expanded; the secret
role  = "operator"               # readonly | operator | admin

[[admin.token]]
name  = "billing-automation"
token = "${ADMIN_TOKEN_BILLING}"
role  = "admin"
```

**Fail-loud at load (CLAUDE.md §4.6):**
- `[admin]` present but **no `[[admin.token]]`** → error (an admin
  listener with no tokens can't authenticate anyone — refuse rather than
  silently lock everyone out *or* run open).
- A token with an unknown `role` string → error.
- Empty `token` / duplicate `name` → error.
- **No `[admin]` block → `/admin/*` is not served at all** (the daemon
  still runs metrics/health). This is the secure default: admin power is
  off unless explicitly configured.

**`[admin].tls` (optional, recommended for non-loopback binds):** reuse
the existing `[sip.tls]`-style cert/key plumbing so the admin listener
can serve HTTPS — a bearer token over plain HTTP on a routable address
leaks the secret. Validated at load like `[sip.tls]`. (Confirm in §6.)

---

## 6. Decisions

**LOCKED (2026-06-19, via review):**
1. **Mechanism — Bearer API tokens.** Config-defined, stored hashed,
   constant-time compare; `Authorization: Bearer`. mTLS is a possible
   later second mechanism, not in this theme. §2/§4.
2. **Authorization — fixed nested roles** `readonly` ⊂ `operator` ⊂
   `admin`, one per token. §3.
3. **Topology — separate admin listener.** `/admin/*` on `[admin].listen`
   (auth); `/metrics` + `/health` + `/ready` stay on
   `[observability].http_listen` (open). §2.

**To confirm (during the build):**
4. **Token storage in config.** Plaintext (env-expanded) hashed at load —
   *recommended*, easiest to operate — vs requiring operators to supply a
   pre-hashed token. Recommend plaintext-hashed-at-load.
5. **Admin TLS.** Ship `[admin].tls` in this theme (recommended — a
   routable admin bind needs it) or defer and document "loopback/mTLS-
   proxy only." Recommend shipping it.
6. **Exact endpoint→role table.** §3 is the proposed mapping; confirm the
   boundary cases (e.g., is `PUT /admin/log` `admin` or `operator`?).
7. **Back-compat / migration.** Moving `/admin/*` off the observability
   listener is a **breaking change** for any deployment that currently
   POSTs admin requests to `[observability].http_listen`. Pre-1.0, and the
   surface was never meant to be exposed, so a clean break + a loud
   CHANGELOG/upgrade note is acceptable. Confirm we don't dual-serve.

---

## 7. Implementation chunks

1. **Auth core (config + RBAC + middleware).** `[admin]` config
   (`listen`, `[[admin.token]]`, optional `tls`) + validation; token
   hashing/compare; the `readonly`/`operator`/`admin` role model + the
   endpoint→min-role table; the auth+authorize middleware (401/403). Unit
   tests for token compare, role gating, and config validation. No
   listener move yet (middleware testable in isolation).
2. **Separate admin listener.** Stand up the `[admin].listen` HTTP
   server, gate it with the middleware, mount `/admin/*` there, and
   **remove** `/admin/*` from the observability listener. Audit log +
   `siphon_ai_admin_requests_total` metric. Supersede the originate
   "no built-in auth" note.
3. **Docs + tests + release.** Rewrite the `admin.rs` threat model,
   `DEPLOY.md` §security, `CONFIG.md` `[admin]` reference; integration
   tests (curl 401 / 403 / 2xx per role); CHANGELOG with the breaking
   migration note; release.

Targets ~**v0.10.0** — native admin auth is the gate to calling SiphonAI
"production-deployable without a bespoke proxy," which is a minor-version
signal.

---

## 8. Out of scope (this theme)

- **mTLS** as an auth mechanism (tokens only here; mTLS is a clean
  follow-up — the role lookup would key off the client-cert subject
  instead of a token).
- **OIDC / external IdP** integration.
- **Per-token rate limits** (the outbound cap + rate limit already guard
  origination; per-token throttling is a later refinement).
- **The signed audit webhook** — lands with the P0 webhook-durability
  theme so it shares one HMAC signer.
