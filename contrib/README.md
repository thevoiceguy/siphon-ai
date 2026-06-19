# `contrib/`

Operator-facing extras that ship with SiphonAI but aren't part of the
daemon binary.

| Path | What it is |
|---|---|
| `sti-pa-roots.pem` | **Template** for the STIR/SHAKEN trust-anchor bundle. Populate it with the authentic STI-PA root(s) before enabling verification — see below. |
| `fail2ban/` | Drop-in fail2ban filter + jails for the daemon's auth-failure log lines. |

---

## STIR/SHAKEN trust anchors (`sti-pa-roots.pem`)

`[security.stir_shaken].trust_anchors` points at a PEM bundle of the root
certificate(s) that a SHAKEN signing certificate's chain must terminate at
to be trusted. SiphonAI loads and validates this file **at startup** (it
must exist and hold ≥1 PEM certificate when `enabled = true`); it is never
fetched at runtime (plan §9 decision 1).

`sti-pa-roots.pem` as shipped is a **placeholder with no certificates** —
using it unmodified is a deliberate fail-loud. You must replace it with the
authentic anchors.

### Why we don't ship a baked-in root

A trust anchor is security-critical. A stale or incorrect anchor silently
rejects every legitimately signed call, and a wrong-but-accepted anchor
would undermine the whole point of verification. The authentic, current
list must come from the authoritative source and be verified — not vendored
blindly into a release.

### What the anchors are

In the US, the **Secure Telephone Identity Policy Administrator (STI-PA)**
— operated by iconectiv — publishes the list of approved **STI Certification
Authority (STI-CA)** root certificates. Authorized service providers
retrieve that trusted-root list from the STI-PA trust-list API as defined by
**ATIS-1000080**. That list is your trust-anchor set. It can contain more
than one root and changes over time as CAs are added or removed.

Other jurisdictions (e.g. Canada's CST-GA) have their own policy
administrator; use the anchors that authority publishes.

### How to populate it

1. Obtain the current trusted-root list from your STI-PA account / trust-list
   API (this is part of your STI onboarding — it is not a public download).
2. Verify each certificate's SHA-256 fingerprint against the value the
   STI-PA publishes out of band:

   ```sh
   openssl x509 -in <root>.pem -noout -fingerprint -sha256
   ```

3. Concatenate the verified root(s) into a single PEM file, replacing the
   placeholder content below the marker in `sti-pa-roots.pem` (or build your
   own file — the path is just config).
4. Install it where the daemon can read it and point config at it:

   ```toml
   [security.stir_shaken]
   enabled       = true
   trust_anchors = "/etc/siphon-ai/sti-pa-roots.pem"
   ```

5. Confirm it loads:

   ```sh
   openssl crl2pkcs7 -nocrl -certfile /etc/siphon-ai/sti-pa-roots.pem \
     | openssl pkcs7 -print_certs -noout      # lists each subject — sanity check
   siphon-ai check --config /etc/siphon-ai/config.toml   # fails loud on a bad bundle
   ```

### Keeping it current

The STI-PA root list rotates rarely (years), but it does change. Re-fetch
and re-verify when the STI-PA announces an update, replace the file, and
restart the daemon (the anchors are read at startup; there is no hot
reload). A `min_attestation` gate means a missing/incorrect anchor will
reject calls — monitor `siphon_ai_invites_total{result="rejected_attestation"}`
and the per-call `verstat` (`cert_chain_valid: false`) for a misconfigured
or out-of-date bundle.

See `docs/CONFIG.md` (`[security.stir_shaken]`) and `docs/HEP.md` (the
`verstat` chunk) for the surrounding configuration and observability.
