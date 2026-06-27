# Design: security & abuse hardening (P1 theme)

> **Status: DECISIONS LOCKED (2026-06-25) — §5.** Same design-first cadence
> as graceful shutdown (→ v0.17.0), admin auth (→ v0.10.0), and webhook
> durability (→ v0.11.0): design note → locked decisions → chunked PRs →
> tag-after-merge. The build follows §6; deviations get noted back here.

Theme: **P1 from `docs/ROADMAP.md`** ("Security & abuse hardening"), now the
top open theme since both P0s shipped (release/packaging v0.16.0, graceful
shutdown v0.17.0). This note covers **three** of the four sub-items the
roadmap groups here; STIR/SHAKEN **outbound signing** is explicitly **out of
scope** (large, gated on multi-week STI-CA cert enrollment — gate on demand
in its own theme). The three in scope:

1. **Listener & secret hardening** — `[admin].tls` + secret-manager
   integration (file-based / systemd credentials, beyond `${VAR}`).
2. **Inbound security** — RFC 3261 §22 digest auth on inbound INVITEs +
   per-source INVITE rate-limiting / admission control.
3. **Signed audit-event stream** — tamper-evident admin/security events as a
   signed webhook (and optional HEP) stream for SIEM ingestion.

The headline finding from the code survey: **all three reuse plumbing we
already ship.** None needs an upstream `forge-media` change, and inbound
digest auth needs **no `siphon-rs` PR** — the server-side `DigestAuthenticator`
already exists upstream (§3.2). This theme is mostly config + glue + docs.

---

## 1. The gaps today

### 1.1 Listener & secret hardening

- **Admin listener is plain HTTP.** `AdminServer::start()`
  (`crates/telemetry/src/http.rs:223-261`) binds a bare `TcpListener` and
  serves `hyper::server::conn::http1`. `admin.rs:37-38` says it out loud:
  *"admin listener is plain HTTP … bind it on loopback or front it with TLS
  termination."* A bearer token (0.10.0) over plain HTTP on any routable
  address **leaks the token** to an on-path observer. `build_admin()`
  (`runtime.rs:1168-1189`) already *warns* on a non-loopback bind — but warn
  isn't a fix.
- **Secrets must sit in the environment.** Every secret — admin tokens, SIP
  digest passwords (`[[gateway]]`, `[[register]]`), webhook/CDR HMAC secrets,
  the HEP capture password — flows through `${VAR}` expansion
  (`crates/config/src/env.rs:51-93`, applied once at `lib.rs:96-99`). That
  forces the operator to put plaintext secrets in the process environment
  (visible in `/proc/<pid>/environ`, dumps, supervisor unit files). There is
  no file-based or systemd-credential path.

### 1.2 Inbound security

- **The only inbound gate is the `[[trunk]]` allowlist.** `on_invite`
  (`crates/sip-glue/src/handler.rs:370`) gates a new INVITE through
  `gate.identify(request, ctx)` → `403 Forbidden` if the source isn't an
  allowed peer (`handler.rs:425-441`), matching on `peer_addrs` (IP/CIDR) and
  `from_hosts` (`RawTrunk`, `raw.rs:593-617`). `from_hosts` is a `From:`-URI
  hostname match — **trivially spoofable** by an on-path attacker, and an
  IP allowlist is impossible for a trunk without a static carrier IP.
  `docs/CONFIG.md` already names digest auth the proper *"no trust in the
  network"* answer and marks it post-v1.
- **No admission control.** Nothing bounds the *rate* of new INVITEs from a
  given source. The existing defence is the external `fail2ban` recipe
  (`docs/SECURITY_FAIL2BAN.md`) — reactive, log-scraping, and after the fact.
  An internet-facing trunk has no in-process DoS posture. (Note: outbound
  *already* has a token-bucket `rate_limit_per_sec`, `[outbound]` — there's a
  pattern to mirror.)

### 1.3 Signed audit-event stream

- Admin actions are **logged + metered** (`handle_admin_request`,
  `http.rs:273-334`: `info!(actor=token.name, action, target, result, peer)` +
  `siphon_ai_admin_requests_total{endpoint,role,result}`,
  `metrics.rs:107`) — but the audit trail is **only local stdout/stderr**.
  There's no tamper-evident, shippable stream a SIEM can ingest, and no
  integrity signature, so logs are repudiable and lost on a compromised host.
  The admin-auth design note (`DESIGN_ADMIN_AUTH.md` §4, §8) **explicitly
  deferred** the signed audit webhook to "the theme that owns the HMAC
  signer" — that signer shipped in 0.11.0; this is that theme.

---

## 2. Goals / non-goals

**Goals**
1. **TLS on the admin listener** — `[admin].tls { cert, key }`, reusing the
   existing rustls server-config plumbing, so a routable admin bind doesn't
   leak its bearer token. Hot-reloadable on `SIGHUP` like the SIP TLS config.
2. **Secrets from outside the environment** — resolve config secrets from
   files / systemd credentials, not just `${VAR}`, with the same fail-loud
   semantics and TOML-only config.
3. **Cryptographic inbound caller auth** — optionally challenge inbound
   INVITEs with RFC 3261 §22 digest, verifying against configured
   credentials, so trust no longer rests on a spoofable network identity.
4. **In-process admission control** — per-source INVITE rate limiting that
   sheds abusive load before it reaches route/codec work.
5. **A tamper-evident, shippable audit trail** — admin/security events
   emitted as an HMAC-signed stream (webhook; HEP optional) for SIEM
   ingestion, reusing the 0.11.0 signer.
6. **Observability for each** (CLAUDE.md §4.5) — metrics/logs ship with the
   feature.

**Non-goals (this theme)**
- **STIR/SHAKEN outbound signing** — separate, cert-enrollment-gated theme.
- **mTLS as an admin *auth* mechanism** — `[admin].tls` is server-side TLS
  (confidentiality for the bearer token). Client-cert *authentication* (role
  keyed off cert subject) stays the clean follow-up named in
  `DESIGN_ADMIN_AUTH.md` §8.
- **A native Vault / cloud-KMS client.** File-based + systemd-credential
  resolution covers Vault Agent (templated files), Docker/K8s secrets, and
  systemd `LoadCredential=` without adding a heavy dependency or network
  call on the config path. Native Vault is a possible later additive source.
- **OIDC / external IdP**, per-token admin rate limits — out of scope as in
  the admin-auth theme.
- **No protocol / CDR / config-schema break.** Everything is additive: new
  optional `[admin.tls]`, new secret-source *syntax* (backward compatible),
  new optional `[sip.auth]` / inbound rate-limit config, and a new optional
  audit sink. All **off by default** — existing configs behave identically.

## 3. Design

### 3.1 Admin TLS (`[admin].tls`)

`AdminServer::start()` (`http.rs:223-261`) loops `listener.accept()` then
`http1::Builder::serve_connection(TokioIo::new(stream), …)`. Add an optional
TLS acceptor in front of that stream, exactly as the SIP listener does:

- **Reuse `load_rustls_server_config(cert, key)`** (the helper behind
  `load_sip_tls_server_config`, `runtime.rs:1389-1414`, imported from the
  `sip_transport` crate) to build an `Arc<rustls::ServerConfig>` from PEM
  cert/key paths. Same loader the SIP TLS listener and per-route bridge TLS
  (`crates/bridge/src/tls.rs`) use — one cert/key code path, fail-loud at
  load.
- When `[admin].tls` is set, wrap the accepted `TcpStream` in a
  `tokio_rustls::TlsAcceptor` before handing it to `serve_connection`.
- **Hot reload:** store the server config in the same `Arc<Swappable<…>>`
  shape used for SIP TLS (`runtime.rs:501-522`) so `SIGHUP` can swap the cert
  (renewal) without dropping the listener. (Or mark `[admin.tls]`
  restart-required for v1 simplicity — decision 6, §5.)
- **Config + validation** (`compile_admin`, `compile.rs:1949-1986`): add
  `tls: Option<AdminTls { cert_path, key_path }>`; validate the PEM loads at
  config time (parity with `[sip.tls]`). A non-loopback `[admin].listen`
  *without* `tls` keeps today's startup **warning** (don't hard-fail — a
  reverse-proxy-terminated deployment is still valid). Recommend escalating
  the warning's wording to name token leakage.

### 3.2 Inbound digest auth (`[sip.auth]`)

**Upstream is ready — no PR.** `siphon-rs`'s `sip-auth` crate ships a
server-side `DigestAuthenticator<S: CredentialStore>` (rev `3023963`,
`crates/sip-auth/src/lib.rs:781`) with exactly the surface we need:

- `challenge(request) -> Response` — builds `401`/`407` with
  `WWW-Authenticate`/`Proxy-Authenticate` (nonce, realm, algorithm, qop,
  opaque).
- `verify(request, headers) -> Result<bool>` — validates the
  `Authorization`/`Proxy-Authorization` header, recomputes the response hash,
  constant-time compares; built-in `NonceManager` gives expiry + replay
  protection.
- `challenge_stale(request)` — `stale=true` re-challenge for expired nonces
  (RFC 7616 §3.5).

**Where it slots in.** `on_invite` (`handler.rs:370`) gates in order: drain
(503, `409-419`) → trunk allowlist (403, `425-441`) → route dispatch
(`443-461`). Digest auth inserts **between the trunk gate and route
dispatch** (after we know the source isn't outright denied, before we commit
to a route):

```text
drain? ─► trunk allowlist? ─► [NEW] digest auth? ─► route dispatch
                                 │
                                 ├─ no Authorization header  → challenge() → 401/407
                                 ├─ header present, verify ok → continue
                                 ├─ verify fails              → challenge()  (or 403)
                                 └─ nonce stale              → challenge_stale()
```

- **`CredentialStore`** is the one piece we implement: a small adapter over
  configured inbound credentials (`username → password`/`HA1`). Built once at
  startup, cloned into the handler. Storing **HA1** (`MD5(user:realm:pass)`)
  rather than the plaintext password is the better posture — decision 3, §5.
- **Config** (`[sip.auth]`, new): `enabled`, `realm`, `algorithm`
  (MD5/SHA-256), `qop`, and a credential table (`[[sip.auth.user]]` with
  `username` + `password`/`ha1`, env/secret-resolved). Off by default;
  fail-loud if `enabled` with zero users.
- **Interaction with `[[trunk]]`:** decision 4 (§5) — recommend digest as an
  *additional* gate (trunk allowlist AND digest, both must pass), with a
  per-trunk `auth_required` opt-in so a static-IP carrier trunk can stay
  allowlist-only while a roaming trunk requires digest.
- **Applies to other in-dialog-initiating methods?** Scope v1 to **INVITE**
  (and REGISTER if we ever act as a registrar — we don't inbound today).
  `BYE`/`ACK`/re-INVITE inside an established dialog are not re-challenged.

### 3.3 Per-source INVITE rate limiting / admission control

A token-bucket keyed on **source identity** (`ctx.peer()` IP, available at
`on_invite`; `handler.rs` via `TransportContext`), checked **before** trunk
and digest work so abusive sources are shed as cheaply as possible.

- **Mirror the outbound limiter.** `[outbound].rate_limit_per_sec` is already
  a documented token bucket (`docs/CONFIG.md:315`); reuse the same shape for
  consistency. New `[sip.admission]` (or `[sip.rate_limit]`): a per-source
  `max_per_sec` (+ burst) and an optional global `max_inflight_invites`
  admission cap.
- **Bounded memory.** Per-IP buckets in a size-capped LRU/`DashMap` with idle
  eviction — a rate limiter that itself leaks under a spoofed-source flood is
  no good. Cap the table; when full, fall back to the global cap.
- **Response when limited:** `503 Service Unavailable` + `Retry-After`
  (consistent with the drain reject, §graceful-shutdown), or silently drop
  (cheaper under flood, but less RFC-friendly) — decision 5, §5. Recommend
  `503` for low-rate trips, **drop** above a hard threshold.
- **`siphon-rs` also exposes a `RateLimiter`** coupled into
  `DigestAuthenticator::with_rate_limiter`. That covers *auth-failure*
  rate limiting specifically; our admission limiter is broader (pre-auth,
  per-source) and lives siphon-ai-side. Use the upstream one *additionally*
  for repeated bad-credential sources if cheap — decision 5.

### 3.4 Signed audit-event stream

Reuse the 0.11.0 signer end-to-end. `sign(secret, ts, body) -> "t=…,v1=<hmac>"`
(`crates/http/src/lib.rs:531`) + the `X-SiphonAI-Signature` header +
`RetryingPoster` (durable retry/spool) already power both webhook sinks. The
audit stream is "a webhook sink that carries security events."

- **Event model.** A new `SecurityEvent` / `AdminAuditEvent` type (or a
  variant family on the existing `WebhookEvent` enum,
  `crates/webhooks/src/event.rs`) covering: every authenticated admin action
  (the data already at `http.rs:313-322` — actor=token name, action, target,
  result, peer), plus auth **failures** (401/403), config reloads
  (`siphon_ai_config_reloads_total` already exists), and — if cheap —
  inbound-auth rejections from §3.2.
- **Delivery.** Decision 7 (§5): a **dedicated** `[audit]` sink (its own
  `url` + `secret` + spool) vs. folding audit events into the existing
  lifecycle `[webhooks]` whitelist. Recommend **dedicated** — audit and
  business-lifecycle events have different consumers (SIEM vs. app backend),
  retention, and secrets; mixing them couples two trust domains. Both reuse
  the same `RetryingPoster`, so "dedicated" is config + a second sink
  instance, not new delivery code.
- **HEP option.** The roadmap names "signed webhook / HEP stream." HEP is the
  natural fit for Homer-centric shops; webhook for SIEM. Recommend **webhook
  first** (direct signer reuse), HEP as an additive follow-up — decision 8.
- **Tamper-evidence.** The HMAC over `t=<unix>,<body>` gives integrity +
  freshness per event. A hash-chain (each event includes the prior event's
  signature) would give *sequence* tamper-evidence (detect dropped/reordered
  events), but adds per-sink state — recommend **out of scope v1**, note as a
  follow-up.

### 3.5 Secret-manager integration

Extend the **resolver**, not the format. Today `${VAR}` expands via the
`EnvSource` trait (`env.rs:18-29`) at one chokepoint (`lib.rs:96-99`). Add
**source-prefixed references** inside the same `${…}` syntax so it's
backward compatible and still a single fail-loud pass:

| Syntax | Resolves to | Use case |
|---|---|---|
| `${VAR}` / `${VAR:-default}` | process env (today) | unchanged |
| `${file:/run/secrets/admin_token}` | trimmed file contents | Docker/K8s secrets, Vault Agent templated files |
| `${cred:admin_token}` | `$CREDENTIALS_DIRECTORY/admin_token` contents | systemd `LoadCredential=` / `ImportCredential=` |

- One new resolver dispatching on the `prefix:` (env remains the default when
  no known prefix) — small change at the `expand` call site, no new config
  fields, works for **every** existing secret (admin tokens, SIP passwords,
  HMAC secrets, HEP password) for free.
- **Fail-loud** parity: missing file / unreadable credential / empty value →
  `EnvError`, daemon refuses to start (matches `EnvError::Missing`).
- **No secret logging** — same discipline as today (tokens hashed at load,
  `auth.rs`; nothing echoes the resolved value).
- Decision 2 (§5): exact prefix names (`file:` / `cred:`) and whether to ship
  both or start with `file:` (which alone covers Vault-Agent + Docker/K8s).

## 4. Observability / tests

**Observability** (ships with each feature, CLAUDE.md §4.5):
- **Admin TLS:** reuse existing admin metrics; log TLS handshake failures at
  `warn` (bounded). A `tls=true/false` field on the admin-listener startup
  log line.
- **Digest auth:** `siphon_ai_sip_auth_total{result}` counter
  (`challenged` / `ok` / `failed` / `stale`); `info` on accept,
  `warn` (rate-limited) on repeated failures. Per-call: a CDR field is
  **optional/additive** — record whether the call was digest-authenticated
  (bumps `CDR_VERSION`; gate on whether wanted — decision 9).
- **Rate limiting:** `siphon_ai_invite_admission_total{result}`
  (`accepted` / `rate_limited` / `dropped`) + a gauge for the live per-source
  table size (cardinality-safe — no IP labels). One `warn`/min max when
  shedding, never per-drop (CLAUDE.md §4.7 discipline).
- **Audit stream:** reuse the 0.11.0 delivery metrics
  (`deliveries`/`attempts`/`spool_depth`/`delivery_seconds`) scoped to the
  audit sink; the audit *events themselves* are the observability.

**Tests:**
- Unit: admin TLS config validation (good/missing/bad PEM); secret resolver
  (`env` / `file:` / `cred:` incl. missing→error); digest `CredentialStore`
  adapter + a `challenge`→`verify` round-trip; rate-limiter token-bucket
  (burst, refill, eviction, table cap); audit event serialization + signature
  round-trip (verify with the 0.11.0 verifier).
- Integration (`test-harness/`): curl the admin listener over TLS (cert
  trust → 2xx; plain HTTP → connection reset). A SIPp scenario: INVITE
  without credentials → `401`, INVITE with correct digest → proceeds, wrong
  digest → re-`401`; a flood scenario trips the rate limiter (`503`/drop) and
  recovers. An audit-sink stub (extend `hep-collector-stub`/webhook stub)
  asserting a signed admin action arrives and its signature verifies.

## 5. Decisions — LOCKED (2026-06-25)

1. **Sequencing = three minor releases**, one per sub-item, each internally
   chunked (mirrors the per-theme cadence):
   - **v0.18.0 — Listener & secret hardening** (admin TLS + secret resolver).
     Smallest, pure reuse; unblocks secure routable admin binds first.
   - **v0.19.0 — Inbound security** (digest auth + admission control).
   - **v0.20.0 — Signed audit-event stream.**
   (One-big-release alternative rejected — the three-way split keeps each PR
   stack reviewable and lets v0.18.0 ship value immediately.)
2. **Secret resolver ships both `file:` and `cred:`.** `${file:/path}` (trimmed
   file contents — Docker/K8s secrets, Vault Agent templated files) and
   `${cred:name}` (`$CREDENTIALS_DIRECTORY/name` — systemd `LoadCredential=`).
   `${VAR}` stays the default when no known prefix is present. Backward
   compatible; one resolver, one fail-loud pass.
3. **Digest stores HA1** (`MD5(user:realm:pass)`) — never holds cleartext.
   Config accepts `password` (hashed to HA1 at load) **or** a pre-computed
   `ha1`.
   - **DEVIATION (v0.19.0 build):** stored as **cleartext**, not HA1. The
     upstream `sip-auth` verifier recomputes HA1 from the cleartext password
     on every challenge (`compute_response(.., creds.password(), ..)`); there
     is no HA1-direct path, and reimplementing digest to accept HA1 would
     violate CLAUDE.md §4.8 (use upstream). Cleartext inbound credentials are
     consistent with how `[[gateway]]`/`[[register]]` already hold outbound
     passwords; the v0.18.0 secret resolver (`${file:…}`/`${cred:…}`) keeps
     them out of the config file. `SipAuthUser`'s `Debug` prints a
     non-reversible password fingerprint (not `[REDACTED]`) so a password
     change still trips the SIGHUP restart-required check.
4. **Digest is an AND gate with the trunk allowlist**, opt-in per trunk via
   `auth_required`. Static-IP carrier trunks stay allowlist-only; roaming /
   no-static-IP trunks set `auth_required` and must also pass digest. Both
   gates must pass when both apply.
5. **Rate limiting:** per-source token bucket keyed on `ctx.peer()` IP in a
   capped LRU with idle eviction; `503 + Retry-After` on a normal trip, hard
   **drop** above a configurable threshold (cheaper under flood); a global
   `max_inflight_invites` admission cap as the backstop. siphon-rs's
   auth-failure `RateLimiter` folded into the digest path additionally if
   cheap.
6. **Admin TLS hot-reloads** via the SIP-TLS `Swappable` pattern (cert renewal
   on `SIGHUP` without dropping the listener). Restart-required rejected — TLS
   certs rotate, and SIP TLS already proves the pattern.
7. **Audit stream = a dedicated `[audit]` sink** (its own `url` + `secret` +
   spool), not folded into `[webhooks]`. Audit (SIEM) and lifecycle (app
   backend) events have different consumers, retention, and trust domains;
   both reuse the same `RetryingPoster`, so this is config + a second sink
   instance, not new delivery code.
8. **Audit transport = webhook first** (direct 0.11.0 signer reuse). HEP
   transport is an additive follow-up, not in this theme.
9. **No CDR `authenticated` field — metrics-only.** Digest outcome is captured
   by `siphon_ai_sip_auth_total`; no `CDR_VERSION` bump. (Revisit only if an
   operator needs per-call attribution.)

**Also settled as defaults:** all sub-features **off by default**; TOML-only;
additive config; **no protocol/CDR/schema break** (decision 9 keeps CDR at its
current version). A non-loopback `[admin].listen` without `[admin.tls]` keeps
today's startup *warning* (not a hard fail — reverse-proxy TLS termination
stays valid); the warning's wording escalates to name token leakage.

## 6. Implementation chunks

Grouped by the three releases (decision 1). Each follows
design→chunks→tag-after-merge.

### v0.18.0 — Listener & secret hardening
1. **Secret resolver.** Extend `expand`/`EnvSource` with `file:` (+ `cred:`)
   prefixes; unit tests; `docs/CONFIG.md` secret-sources section. No new
   config fields — works for every existing secret.
2. **Admin TLS.** `[admin].tls { cert, key }` + `compile_admin` validation
   (reuse `load_rustls_server_config`); wrap `AdminServer::start` accept loop
   in a `TlsAcceptor`; `Swappable` for `SIGHUP` (decision 6); escalate the
   non-loopback warning. Integration test (TLS curl). `DEPLOY.md` +
   `CONFIG.md` + threat-model note in `admin.rs`.

### v0.19.0 — Inbound security
3. **Digest auth.** `[sip.auth]` config + `CredentialStore` adapter (HA1);
   wire `challenge`/`verify`/`challenge_stale` into `on_invite` between trunk
   and route; `siphon_ai_sip_auth_total` metric; SIPp 401→auth→proceed
   scenario; `docs/CONFIG.md` + a `docs/SECURITY_*` digest section.
4. **Admission control.** `[sip.admission]` per-source token bucket + global
   inflight cap (capped LRU, idle eviction); `503`/drop policy;
   `siphon_ai_invite_admission_total` metric; SIPp flood scenario;
   `docs/CONFIG.md` + cross-link the fail2ban recipe (now defence-in-depth,
   not the only line).

### v0.20.0 — Signed audit-event stream
5. **Audit sink core.** `SecurityEvent`/`AdminAuditEvent` model; a dedicated
   `[audit]` sink reusing `RetryingPoster` + `sign()` +
   `X-SiphonAI-Signature`; fire from `handle_admin_request` (success +
   401/403) and config-reload; serialization + signature round-trip tests.
6. **Docs + release.** `docs/DEPLOY.md` SIEM-ingestion section, payload schema
   + signature-verification recipe, audit-sink stub in `test-harness/`,
   CHANGELOG, tag. (HEP transport + hash-chain noted as follow-ups.)

## 7. Out of scope (this theme)

- **STIR/SHAKEN outbound signing** — separate cert-gated theme.
- **mTLS admin auth**, OIDC/IdP, per-token admin rate limits — as in the
  admin-auth theme.
- **Native Vault / cloud-KMS client** — `file:`/`cred:` cover the templated-
  file integration path; a native client is a later additive source.
- **Audit hash-chaining** and **HEP audit transport** — additive follow-ups
  after the webhook audit sink lands.
- **Per-call digest CDR attribution** unless decision 9 opts in.
