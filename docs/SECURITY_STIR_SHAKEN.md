# STIR/SHAKEN security model

What SiphonAI's call-authentication actually proves, what it doesn't, and
how to operate it. Read this before turning on a `min_attestation` gate.

For configuration mechanics see [`CONFIG.md`](CONFIG.md) `[security]`; for
the wire shape see [`PROTOCOL.md`](PROTOCOL.md) (`verstat`); for the Twilio
cross-check recipe see [`INTEGRATIONS_TWILIO.md`](INTEGRATIONS_TWILIO.md).

## 1. Attestation is a *signal*, not a verdict

STIR/SHAKEN tells you **who signed for the call**, not **whether the call
is trustworthy**. A full **A** attestation means the originating provider
authenticated its customer *and* confirmed they're authorized to use the
calling number. It does **not** mean the call is wanted, legal, or not
fraud:

- A scammer who legitimately owns (or legally leases) a number gets **A**
  on their calls. Attestation is about provenance of the *number*, not the
  *intent* of the caller.
- **B** (partial) and **C** (gateway) are weaker still — the provider
  authenticated the origin but not the number, or only the ingress point.

Treat attestation as one input to a fraud/answer decision — alongside
reputation, rate, dialed-number patterns, and your own allowlists — never
as the whole decision. SiphonAI gives you the verified signal and a gate to
act on it; the policy is yours.

## 2. Two independent trust domains (don't conflate them)

Verifying one inbound call touches **two separate sets of trust roots**.
Mixing them up is the most common operational mistake.

| | Trusts | Configured by | What it secures |
|---|---|---|---|
| **Fetching the `x5u` cert** (HTTPS GET) | The public web PKI (Mozilla roots, built in) **plus** `x5u_tls_extra_ca` | `x5u_tls_extra_ca` (optional) | Transport integrity of the cert download |
| **Validating the SHAKEN chain** | Your STI-PA trust anchors | `trust_anchors` (required when enabled) | That an authority you trust vouches for the signing key |

So the certificate **fetched from** `x5u` must chain to `trust_anchors`,
while the HTTPS server **hosting** `x5u` must present a TLS cert trusted by
the web PKI (or by `x5u_tls_extra_ca`). A self-signed HTTPS endpoint for
`x5u` fails the *fetch* before chain validation ever runs — set
`x5u_tls_extra_ca` only when you privately host `x5u` (lab/staging), and
understand it widens *fetch* trust only, never the SHAKEN chain.

## 3. What each `verstat` field means

`start.verstat` (and the CDR / HEP copies) carries the full breakdown so a
consumer can see *why* a call did or didn't pass:

| Field | Meaning |
|---|---|
| `attest` | The attestation level **claimed** in the PASSporT (`A`/`B`/`C`). Untrusted on its own. |
| `orig_tn` | The originating TN from the `orig` claim. |
| `orig_passed` | `orig.tn` matched the SIP `From` user. |
| `dest_passed` | A `dest.tn` matched the SIP `To` / request URI. |
| `cert_chain_valid` | The signing cert chained to a configured STI-PA anchor. |
| `signature_valid` | The ES256 signature over the PASSporT verified under that cert. |
| `iat_passed` | The PASSporT `iat` was within the freshness window (replay protection). |
| `error` | Human-readable reason when something didn't pass. |

**Trust rule:** the claimed `attest` is meaningful only when the call
*fully passed* — every boolean true. SiphonAI's gate uses this composite
(`passed()` → `trusted_attestation()`), never the raw `attest`. A WS server
applying its own policy must do the same: a present `attest: "A"` with
`signature_valid: false` is an **unverified claim**, not an A call.

## 4. Replay / freshness (`iat`)

A captured-and-replayed PASSporT would otherwise re-authenticate a spoofed
call. The `iat` (issued-at) freshness check rejects PASSporTs whose `iat`
is outside `iat_freshness_secs` of now — in the past (stale/replayed) or
the future (clock skew). A missing `iat` fails (the claim is required).

This bounds, it doesn't eliminate, replay: an attacker within the window
who can also spoof the SIP `From`/`To` to match `orig`/`dest` could still
replay. Keep the window tight (default 60 s) and keep your NTP healthy.
`iat_freshness_secs = 0` disables the check — only for upstreams with
provably broken clocks, and understand the replay exposure it reopens.

## 5. The policy gate

When you're ready to *act* on the verdict (not just observe), the gate
rejects before any media is allocated:

- `require_identity = true` → an INVITE with **no** `Identity` header is
  rejected **428 Use Identity Header**.
- `min_attestation = "A" | "B" | "C"` → a call whose *trusted* attestation
  is below the floor is rejected with `min_attestation_response`
  (**403** default, or 488 / 606), carrying `Reason: Q.850;cause=21`.
- Per-route override: `[route.security].min_attestation` (strict — fully
  replaces the global for matching calls).

Roll it out **observe-first**: leave `min_attestation = "none"` while you
confirm verdicts look right (see §6 and the Twilio cross-check recipe),
then raise the floor. A non-`none` floor with a stale/missing
`trust_anchors` bundle rejects **every** call — which is why the bundle is
validated loudly at startup and ships as an unpopulated template you must
fill in ([`contrib/README.md`](../contrib/README.md)).

## 6. What to monitor

- `siphon_ai_verstat_total{result="passed|failed|unsigned"}` — the verdict
  mix. A sudden swing to `failed` after a config change usually means a
  trust-anchor or clock problem, not an attack.
- `siphon_ai_invites_total{result="rejected_attestation"}` — gate
  rejections; alertable.
- Per-call detail: the `verstat` on the WS `start`, the CDR
  (`verstat_attest` / `verstat_passed`), and the HEP `Verstat` chunk in
  Homer (correlated by Call-ID).
- The `error` string distinguishes *why* a call failed (bad chain, bad
  signature, TN mismatch, stale `iat`, unreachable `x5u`).

During initial rollout, the [`verstat-compare`](../examples/verstat-compare-py/)
example logs your verdict against a carrier's `X-Twilio-VerStat` so you can
spot divergence before gating.

## 7. Limitations (what 0.4.x does NOT do)

- **No revocation.** CRL/OCSP of the signing cert isn't consulted — a
  revoked-but-unexpired STI cert still validates. (Deferred; see
  `DEV_PLAN_0.4.1.md` §8.)
- **No outbound attestation.** SiphonAI verifies inbound only; it does not
  sign its own outbound calls (predicated on outbound origination, post-v1).
- **TN-AuthList authorization isn't cross-checked** beyond the `orig`/`dest`
  claim-to-SIP binding — RFC 8226 TN-Authorization-List ↔ number scoping is
  not enforced.
- **`x5u` fetch is best-effort and bounded** (https-only, redirect-free,
  64 KiB / 5 s caps, TTL-cached). An unreachable `x5u` yields a failed
  verdict, not a retry storm.
- **Attestation ≠ trust** (§1). Bears repeating.

## 8. Defaults are safe

Everything here is **off by default**. A deployment that doesn't set
`[security.stir_shaken].enabled = true` surfaces no `verstat`, runs no
verification, and gates nothing — identical to pre-0.4.0 behaviour. Turning
it on is opt-in, and the gate stays open (`min_attestation = "none"`) until
you choose to close it.
