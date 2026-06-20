# SiphonAI 0.4.1 Development Plan

**Theme: finish the STIR/SHAKEN story — close the 0.4.0 deferrals before 0.5.0.**

0.4.0 shipped inbound STIR/SHAKEN verification, the attestation policy
gate, the verstat surfaces (WS `start`, CDR, HEP), and the trust-anchor
template — all off by default. Four items slipped (tracked in
`DEV_PLAN_0.4.0.md` §7/§10). 0.4.1 lands them so the feature is complete
and operator-documented before we open 0.5.0.

This is a **point release**, not a new theme: hardening + completion of the
0.4.x call-authentication line. Everything stays **off by default** and the
WS protocol stays at `version: "1"` (the one new verstat field is additive).

## 1. Cardinal rule, restated

Still no AI code; verification stays in `sip-identity` (crypto) +
`siphon-ai-stir-shaken` (fetch/cache/orchestration). New behaviour is
gated behind `[security.stir_shaken].enabled = true`. Observability ships
with each change (CLAUDE.md §4.5).

## 2. Scope (must-have)

The four 0.4.0 deferrals, plus the one small feature that unblocks testing
them:

1. **`iat` freshness check** — reject replayed/stale PASSporTs.
2. **`x5u_tls_extra_ca` config** — supplemental CA roots for the `x5u`
   HTTPS fetch (enables private/lab x5u hosting **and** the CI passing
   scenario; see §5 decision 2).
3. **Passing-attestation SIPp scenario** + a `gen-test-passport` helper —
   the first end-to-end *green* verstat path under CI.
4. **Twilio Caller Identity recipe** — compare our verdict against
   Twilio's `X-Twilio-VerStat` header.
5. **`docs/SECURITY_STIR_SHAKEN.md`** — the security-model doc (attestation
   is a signal, not a verdict; the two trust domains; threat model).

## 3. Item-by-item approach

### 3.1 `iat` freshness (app-side)

ATIS-1000074 verification rejects a PASSporT whose `iat` is outside a small
freshness window (replay protection). It lands in `siphon-ai-stir-shaken`,
consistent with the `orig`/`dest` claim checks already done there (decision
1).

- `siphon-ai-security`: add `iat_passed: bool` to `VerificationResult`, and
  fold it into the `passed()` composite (so a stale PASSporT can't yield a
  trusted attestation). Update `unsigned()` and all constructors.
- `siphon-ai-stir-shaken`: in `build_result`, compute `iat_passed` from
  `passport.claims.iat` vs `now` against the configured window. A **missing**
  `iat` fails the check (the claim is required). `iat` in the future beyond
  the window also fails (clock-skew / replay).
- Config: `[security.stir_shaken].iat_freshness_secs` (u64, default `60`).
  `0` disables the check (admits any `iat`) for operators with broken
  upstream clocks — documented escape hatch.
- Surfaces: `iat_passed` rides `start.verstat` (additive) and feeds the
  existing `verstat_passed` CDR composite; the HEP verstat JSON carries it
  for free. The `error` string names an `iat` failure.
- Tests: fresh passes; stale fails; future-skew fails; missing-`iat` fails;
  `iat_freshness_secs = 0` disables.

**Behaviour-change note:** with verification enabled, a call that passed in
0.4.0 but carries a stale `iat` will now fail. This is the spec-correct
outcome; it only affects deployments that opted into `stir_shaken`, and the
window/disable knob gives operators control. Called out in the CHANGELOG.

### 3.2 `x5u_tls_extra_ca` config

The verifier fetches `x5u` over HTTPS and currently trusts only the public
web PKI (`webpki-roots`). Operators hosting `x5u` behind a private CA — and
our own CI passing test — need to add a CA for that fetch.

- Config: `[security.stir_shaken].x5u_tls_extra_ca` (optional PEM path).
  Validated at load (exists + ≥1 cert) when set.
- `siphon-ai-stir-shaken`: when set, load the PEM and
  `reqwest::ClientBuilder::add_root_certificate(...)` for each cert. This is
  **additive** to the default web roots, not a replacement, and applies
  **only** to the `x5u` fetch TLS — never to the SHAKEN chain (that stays
  the STI-PA `trust_anchors`). Keep the redirect-free / size / time caps.
- Docs must make the two-trust-domain distinction explicit (it's the #1
  source of confusion) and warn this widens fetch trust.
- Tests: config validation; verifier builds with the extra CA; unit-level
  assertion that an extra root is accepted.

### 3.3 Passing-attestation SIPp scenario + `gen-test-passport`

The first *green* verstat path under CI. Needs a self-contained mini STI
ecosystem, trusted via 3.2.

- `gen-test-passport` helper (small Rust bin under `tools/`, or a Python
  script reusing the echo-ws venv): emits
  - `anchor-ca.pem` (STI-PA root) + leaf signing cert/key,
  - `x5u-tls-ca.pem` + an HTTPS server cert/key for the x5u host,
  - the leaf cert served at the `x5u` path,
  - a fully ES256-signed `Identity:` header (header `x5u=https://127.0.0.1:PORT/...`,
    `orig.tn`/`dest.tn` matching the SIPp `From`/`To`).
  Doubles as the operator "lab full-pass" tool.
- `run-all.sh` stir_shaken phase: start the HTTPS x5u server, run the daemon
  with `enabled`, `trust_anchors = anchor-ca`, `x5u_tls_extra_ca = x5u-tls-ca`,
  `min_attestation = "A"`; send the signed INVITE → expect the call to be
  **admitted** (200/ACK/…/BYE via the echo WS), proving `verstat_passed = true`.
  Sits alongside the existing 428/403 reject scenarios.
- Keeps cert validity windows current (generated at run time).

### 3.4 Twilio Caller Identity recipe

Show operators how to cross-check our verdict against Twilio's
`X-Twilio-VerStat` header (e.g. `TN-Validation-Passed-A`).

- Mechanism: `[bridge].forward_headers = ["X-Twilio-VerStat"]` surfaces the
  SIP header to the WS server on `start.sip.headers`; the server compares it
  against `start.verstat.attest` and logs agreement/disagreement.
- Deliverable: a recipe doc (extend `examples/twilio-trunk/` or
  `docs/`), plus a small WS-server comparison snippet. Documentation +
  example, no daemon code.

### 3.5 `docs/SECURITY_STIR_SHAKEN.md`

The security-model doc promised in 0.4.0 §11.

- Attestation is a **signal, not a verdict** (an A-attested call from a bad
  actor who controls a valid number is still A).
- The **two trust domains**: web PKI (+ `x5u_tls_extra_ca`) for the fetch
  vs STI-PA `trust_anchors` for the chain.
- What each `verstat` boolean means; how `passed()`/`trusted_attestation()`
  gate; replay/freshness (`iat`).
- Policy tuning (`min_attestation`, `require_identity`, per-route) and what
  to monitor (`siphon_ai_verstat_total`, `rejected_attestation`).
- Limitations + threat model.

## 4. Observability / protocol

- New verstat field `iat_passed` — additive; protocol stays `version: "1"`.
- No new metrics required (the `iat` failure rolls into
  `siphon_ai_verstat_total{result="failed"}`); the `error` string
  distinguishes the cause. CDR schema stays at version 1.

## 5. Decisions (locked)

1. ☑ **`iat` freshness lives app-side** in `siphon-ai-stir-shaken` (no
   upstream `sip-identity` PR), config `iat_freshness_secs` (default 60,
   `0` disables), adding `iat_passed` to `VerificationResult`.
2. ☑ **Add `x5u_tls_extra_ca`** (supplemental fetch-TLS roots via reqwest
   `add_root_certificate`) so private/lab x5u hosting works and the passing
   SIPp scenario is **CI-gated**.
3. ☑ **Point release `0.4.1`** — completion/hardening of the 0.4.x line.
   `iat` is a verdict-affecting change but gated behind `enabled` + the
   window knob; additive otherwise.

## 6. Chunk sequence (each a PR off `main`)

Land bottom-up; avoid the down-stack squash tangle from 0.4.0 (land each on
`main` before basing the next, or keep them independent where possible).

| # | Chunk | Touches |
|---|---|---|
| 1 | `iat` freshness | `security`, `stir-shaken`, `config`, docs, tests |
| 2 | `x5u_tls_extra_ca` | `stir-shaken`, `config`, docs, tests |
| 3 | passing SIPp scenario + `gen-test-passport` | `tools/`, `test-harness/`, `run-all.sh` (needs #2) |
| 4 | Twilio Caller Identity recipe | `examples/`/`docs/` |
| 5 | `docs/SECURITY_STIR_SHAKEN.md` | docs (references #1–#4) |
| 6 | Release 0.4.1 | version bump, CHANGELOG, README, tag |

Chunks 1, 2, 4, 5 are largely independent; 3 depends on 2.

## 7. Definition of Done — v0.4.1

1. A PASSporT with a stale `iat` is rejected (verstat `iat_passed: false`,
   `verstat_passed: false`); `iat_freshness_secs` tunes the window and `0`
   disables it.
2. An operator can point `x5u` at a privately-hosted endpoint by supplying
   `x5u_tls_extra_ca`, and the docs make the fetch-TLS vs chain-trust
   distinction explicit.
3. CI gates a **passing** STIR/SHAKEN call end-to-end (green verstat →
   admitted), alongside the existing 428/403 reject scenarios.
4. The Twilio Caller Identity recipe lets an operator confirm our verdict
   agrees with `X-Twilio-VerStat`.
5. `docs/SECURITY_STIR_SHAKEN.md` explains the trust model, threat model,
   and policy tuning.
6. Released as `v0.4.1`; upgrade from 0.4.0 is config-compatible (new
   fields optional, defaults preserve behaviour except the spec-correct
   `iat` rejection for opted-in deployments).

## 8. Out of scope (still deferred past 0.4.1)

- Upstream `sip-identity` `iat`/freshness API (we do it app-side).
- CRL/OCSP revocation of signing certs.
- Outbound STIR/SHAKEN attestation generation (we sign) — 0.5.0+, predicated
  on outbound origination.
- Multi-trust-anchor-per-policy and per-originator verdict caching (see
  `DEV_PLAN_0.4.0.md` §6).
