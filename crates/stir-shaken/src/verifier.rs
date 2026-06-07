//! The STIR/SHAKEN verifier: parse → fetch → chain-validate → verdict.
//!
//! [`Verifier`] is the one stateful object the accept path holds. Given the
//! raw `Identity` header value off an inbound INVITE plus the SIP `From` and
//! `To` user parts, [`Verifier::verify`] produces a
//! [`VerificationResult`] — the verstat verdict the rest of the system
//! surfaces (on `BridgeOut::Start`, the CDR, and HEP) and gates on.
//!
//! It owns the pieces `sip-identity` deliberately left to the application:
//! the `x5u` HTTPS fetch, its TTL cache, and the trust anchors the chain is
//! validated against. The cryptographic core (ES256 + X.509 chain
//! validation) lives in `sip-identity`; this crate orchestrates it and adds
//! the SHAKEN claim checks (`orig.tn`/`dest.tn` vs the SIP identities).
//!
//! ```no_run
//! # async fn doc(cfg: &siphon_ai_security::StirShakenConfig) -> Result<(), Box<dyn std::error::Error>> {
//! use siphon_ai_stir_shaken::Verifier;
//! let verifier = Verifier::from_config(cfg)?;
//! let verstat = verifier
//!     .verify(identity_header_value, "+12155551212", "+12155551213")
//!     .await;
//! if verstat.passed() {
//!     // trusted attestation available via verstat.trusted_attestation()
//! }
//! # let _ = verstat; Ok(())
//! # }
//! # const identity_header_value: &str = "";
//! ```

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rustls_pki_types::UnixTime;
use sip_identity::{IdentityError, IdentityHeader, Passport, VerifyError};
use siphon_ai_security::{AttestationLevel, StirShakenConfig, VerificationResult};
use thiserror::Error;
use tracing::{debug, warn};

use crate::cache::CertCache;
use crate::fetch::{fetch_chain, FetchError};
use crate::pem::{load_trust_anchors, TrustAnchorLoadError};

/// Per-request timeout for the `x5u` certificate fetch. Bounds how long a
/// slow or hostile signer can stall the accept path before we give up and
/// return an unverified verdict.
const FETCH_TIMEOUT: Duration = Duration::from_secs(5);

/// Failure constructing a [`Verifier`].
#[derive(Debug, Error)]
pub enum BuildError {
    /// The trust-anchor PEM file could not be loaded.
    #[error("loading trust anchors: {0}")]
    TrustAnchors(#[from] TrustAnchorLoadError),
    /// The HTTP client could not be built (TLS backend init, …).
    #[error("building HTTP client: {0}")]
    HttpClient(String),
    /// A supplemental x5u-TLS CA certificate could not be parsed.
    #[error("x5u_tls_extra_ca certificate invalid: {0}")]
    ExtraCa(String),
}

/// Verifies inbound `Identity` headers against the STI-PA trust anchors.
///
/// Cheap to clone — the trust anchors and cache are shared behind `Arc`,
/// and `reqwest::Client` is itself reference-counted — so the accept path
/// can hold one and clone per call, or wrap it in `Arc`, either way.
#[derive(Clone)]
pub struct Verifier {
    trust_anchors: Arc<Vec<Vec<u8>>>,
    cache: Arc<CertCache>,
    http: reqwest::Client,
    cache_ttl: Duration,
    /// PASSporT `iat` freshness window; `Duration::ZERO` disables the check.
    iat_freshness: Duration,
}

impl Verifier {
    /// Build a verifier from already-decoded trust-anchor DERs, a cache TTL,
    /// an `iat` freshness window (`Duration::ZERO` disables the freshness
    /// check), and optional supplemental CA DERs trusted **for the `x5u`
    /// HTTPS fetch only** (added to the public web-PKI roots; empty = public
    /// roots only). Prefer [`Verifier::from_config`] from daemon code; this
    /// is the seam for tests and embedders that hold anchors in memory.
    pub fn new(
        trust_anchors: Vec<Vec<u8>>,
        cache_ttl: Duration,
        iat_freshness: Duration,
        x5u_extra_roots: Vec<Vec<u8>>,
    ) -> Result<Self, BuildError> {
        let mut builder = reqwest::Client::builder()
            .timeout(FETCH_TIMEOUT)
            // x5u is a direct cert URL; never chase a redirect to an
            // attacker-chosen or internal host.
            .redirect(reqwest::redirect::Policy::none());
        for der in &x5u_extra_roots {
            // Additive to the default web roots, applied to the fetch TLS
            // handshake only — never to the SHAKEN chain (that's `anchors`).
            let cert = reqwest::Certificate::from_der(der)
                .map_err(|e| BuildError::ExtraCa(e.to_string()))?;
            builder = builder.add_root_certificate(cert);
        }
        let http = builder
            .build()
            .map_err(|e| BuildError::HttpClient(e.to_string()))?;
        Ok(Self {
            trust_anchors: Arc::new(trust_anchors),
            cache: Arc::new(CertCache::new()),
            http,
            cache_ttl,
            iat_freshness,
        })
    }

    /// Build a verifier from the compiled `[security.stir_shaken]` config:
    /// load + decode the trust-anchor PEM file, the optional supplemental
    /// x5u-TLS CA bundle, and adopt the configured cache TTL and `iat`
    /// freshness window. Fails loud if either PEM file is unreadable or empty.
    pub fn from_config(cfg: &StirShakenConfig) -> Result<Self, BuildError> {
        let anchors = load_trust_anchors(&cfg.trust_anchors)?;
        let x5u_extra_roots = match &cfg.x5u_tls_extra_ca {
            Some(path) => load_trust_anchors(path)?,
            None => Vec::new(),
        };
        Self::new(
            anchors,
            cfg.cert_cache_ttl,
            cfg.iat_freshness,
            x5u_extra_roots,
        )
    }

    /// Verify the `Identity` header of an inbound INVITE.
    ///
    /// - `identity_value` — the raw `Identity` header value (everything
    ///   after `Identity:`).
    /// - `from_user` — the SIP `From` user part (the claimed caller).
    /// - `to_user` — the SIP `To` / Request-URI user part (the callee).
    ///
    /// Never errors: every failure mode (unparseable header, unreachable
    /// `x5u`, broken chain, bad signature, claim mismatch) maps to a
    /// [`VerificationResult`] with the relevant booleans `false` and a
    /// human-readable `error`, so the policy gate always has a verdict.
    #[tracing::instrument(skip_all)]
    pub async fn verify(
        &self,
        identity_value: &str,
        from_user: &str,
        to_user: &str,
    ) -> VerificationResult {
        let header = match IdentityHeader::parse(identity_value) {
            Ok(h) => h,
            Err(e) => {
                debug!(error = %e, "Identity header failed to parse");
                return parse_failure(e);
            }
        };

        let now = now_unix();
        let x5u = header.passport.header.x5u.clone();
        let chain = match self.resolve_chain(&x5u).await {
            Ok(chain) => chain,
            Err(e) => {
                warn!(%x5u, error = %e, "x5u certificate fetch failed");
                return fetch_failure(
                    &header.passport,
                    from_user,
                    to_user,
                    e,
                    now,
                    self.iat_freshness,
                );
            }
        };

        build_result(
            &header.passport,
            from_user,
            to_user,
            &chain,
            &self.trust_anchors,
            now,
            self.iat_freshness,
        )
    }

    /// Return the certificate chain for `x5u`, from cache when fresh,
    /// otherwise fetched and cached for [`cache_ttl`](Self::cache_ttl).
    ///
    /// Note: concurrent misses for the same `x5u` may each fetch (no
    /// single-flight de-dup) — a brief, bounded redundancy we accept for a
    /// lock-free fetch path; the steady state is a cache hit.
    async fn resolve_chain(&self, x5u: &str) -> Result<Arc<Vec<Vec<u8>>>, FetchError> {
        if let Some(chain) = self.cache.get(x5u, Instant::now()) {
            debug!(%x5u, "x5u cache hit");
            return Ok(chain);
        }
        let chain = Arc::new(fetch_chain(&self.http, x5u).await?);
        self.cache.insert(
            x5u.to_string(),
            chain.clone(),
            Instant::now() + self.cache_ttl,
        );
        Ok(chain)
    }
}

/// A verdict for a present-but-unparseable `Identity` header. Distinct from
/// [`VerificationResult::unsigned`] (no header at all) only by the `error`.
fn parse_failure(e: IdentityError) -> VerificationResult {
    VerificationResult {
        error: Some(format!("Identity header parse failed: {e}")),
        ..VerificationResult::unsigned()
    }
}

/// A verdict for a header that parsed but whose `x5u` could not be fetched.
/// The claim-derived fields (attestation, `orig.tn`, and the TN matches)
/// are still populated — they don't depend on the certificate — but nothing
/// cryptographic could be checked.
fn fetch_failure(
    passport: &Passport,
    from_user: &str,
    to_user: &str,
    e: FetchError,
    now: UnixTime,
    iat_freshness: Duration,
) -> VerificationResult {
    VerificationResult {
        attest: passport.claims.attest.map(map_attest),
        orig_tn: passport.claims.orig_tn.clone(),
        orig_passed: orig_matches(passport, from_user),
        dest_passed: dest_matches(passport, to_user),
        cert_chain_valid: false,
        signature_valid: false,
        iat_passed: iat_fresh(passport.claims.iat, now, iat_freshness),
        error: Some(format!("x5u certificate fetch failed: {e}")),
    }
}

/// Assemble the full verdict once the certificate chain is in hand: run the
/// chain + signature verification (`sip-identity`) and the SHAKEN claim
/// checks, then fold both into a [`VerificationResult`]. Pure given the
/// chain, anchors, and `now` — the unit-test seam for the whole verdict.
fn build_result(
    passport: &Passport,
    from_user: &str,
    to_user: &str,
    chain: &[Vec<u8>],
    anchors: &[Vec<u8>],
    now: UnixTime,
    iat_freshness: Duration,
) -> VerificationResult {
    let orig_passed = orig_matches(passport, from_user);
    let dest_passed = dest_matches(passport, to_user);
    let iat_passed = iat_fresh(passport.claims.iat, now, iat_freshness);
    let (cert_chain_valid, signature_valid, crypto_err) =
        run_chain_verify(passport, chain, anchors, now);

    // Headline reason, in priority order: a crypto failure, then a TN-claim
    // mismatch, then a stale `iat` — so a fully-valid call that fails only
    // freshness still says why it didn't pass.
    let error = crypto_err
        .or_else(|| claim_mismatch(orig_passed, dest_passed))
        .or_else(|| (!iat_passed).then(|| iat_error(passport.claims.iat)));

    VerificationResult {
        attest: passport.claims.attest.map(map_attest),
        orig_tn: passport.claims.orig_tn.clone(),
        orig_passed,
        dest_passed,
        cert_chain_valid,
        signature_valid,
        iat_passed,
        error,
    }
}

/// Whether the PASSporT `iat` is within `window` of `now` (past or future).
/// A zero `window` disables the check (always fresh). A missing `iat` is
/// never fresh — ATIS-1000074 requires the claim, so its absence is a
/// verification failure, not a pass.
fn iat_fresh(iat: Option<i64>, now: UnixTime, window: Duration) -> bool {
    if window.is_zero() {
        return true;
    }
    let Some(iat) = iat else {
        return false;
    };
    let skew = (now.as_secs() as i64 - iat).unsigned_abs();
    skew <= window.as_secs()
}

/// Human-readable reason for an `iat` freshness failure.
fn iat_error(iat: Option<i64>) -> String {
    match iat {
        Some(_) => "PASSporT iat outside the freshness window".to_string(),
        None => "PASSporT has no iat claim".to_string(),
    }
}

/// Validate the chain and verify the PASSporT signature under the leaf,
/// translating [`VerifyError`] into the two booleans the verdict carries
/// plus an optional reason. The chain is `[leaf, intermediates…]` as
/// returned by the fetch.
///
/// `sip-identity` checks the chain *before* the signature, so the booleans
/// reflect how far it got: `ChainInvalid` ⇒ both false (signature never
/// reached); `SignatureInvalid` ⇒ chain valid but signature false.
fn run_chain_verify(
    passport: &Passport,
    chain: &[Vec<u8>],
    anchors: &[Vec<u8>],
    now: UnixTime,
) -> (bool, bool, Option<String>) {
    let Some((leaf, intermediates)) = chain.split_first() else {
        return (
            false,
            false,
            Some("x5u returned no certificates".to_string()),
        );
    };
    let intermediate_refs: Vec<&[u8]> = intermediates.iter().map(Vec::as_slice).collect();
    let anchor_refs: Vec<&[u8]> = anchors.iter().map(Vec::as_slice).collect();

    match passport.verify_with_chain(leaf, &intermediate_refs, &anchor_refs, now) {
        Ok(()) => (true, true, None),
        Err(VerifyError::ChainInvalid(m)) => (
            false,
            false,
            Some(format!("certificate chain invalid: {m}")),
        ),
        Err(VerifyError::SignatureInvalid) => (
            true,
            false,
            Some("PASSporT signature did not verify".to_string()),
        ),
        Err(VerifyError::CertParse(m)) => {
            (false, false, Some(format!("certificate parse failed: {m}")))
        }
        Err(VerifyError::UnsupportedAlg(a)) => {
            (false, false, Some(format!("unsupported PASSporT alg: {a}")))
        }
        Err(VerifyError::MalformedKey { len }) => (
            false,
            false,
            Some(format!("malformed signing key ({len} bytes)")),
        ),
    }
}

/// Does the PASSporT `orig.tn` claim match the SIP `From` user?
fn orig_matches(passport: &Passport, from_user: &str) -> bool {
    passport
        .claims
        .orig_tn
        .as_deref()
        .is_some_and(|tn| tn_matches(tn, from_user))
}

/// Does any PASSporT `dest.tn` claim match the SIP `To`/R-URI user?
fn dest_matches(passport: &Passport, to_user: &str) -> bool {
    passport
        .claims
        .dest_tns
        .iter()
        .any(|tn| tn_matches(tn, to_user))
}

/// Compare a PASSporT telephone-number claim against a SIP user part on
/// digits only. SHAKEN TNs are E.164 (`+1…`) while a SIP user part may
/// carry a `+`, visual separators, or none — so `+12155551212`,
/// `12155551212`, and `1-215-555-1212` all compare equal. An empty digit
/// string never matches (guards two blank inputs comparing equal).
fn tn_matches(claim: &str, sip_user: &str) -> bool {
    let claim_digits = digits(claim);
    !claim_digits.is_empty() && claim_digits == digits(sip_user)
}

fn digits(s: &str) -> String {
    s.chars().filter(char::is_ascii_digit).collect()
}

/// Describe a TN-binding failure when the cryptography otherwise passed.
fn claim_mismatch(orig_passed: bool, dest_passed: bool) -> Option<String> {
    match (orig_passed, dest_passed) {
        (true, true) => None,
        (false, true) => Some("orig.tn did not match SIP From".to_string()),
        (true, false) => Some("dest.tn did not match SIP To".to_string()),
        (false, false) => Some("orig.tn and dest.tn did not match SIP From/To".to_string()),
    }
}

/// Map the `sip-identity` attestation enum onto the `siphon-ai-security`
/// one. They share the A/B/C SHAKEN wire characters; this keeps the two
/// vocabularies independent while bridging them at the one seam.
fn map_attest(level: sip_identity::AttestationLevel) -> AttestationLevel {
    match level {
        sip_identity::AttestationLevel::A => AttestationLevel::A,
        sip_identity::AttestationLevel::B => AttestationLevel::B,
        sip_identity::AttestationLevel::C => AttestationLevel::C,
    }
}

/// Current wall-clock time as the `UnixTime` the chain validity check wants.
/// A clock before the Unix epoch (impossible in practice) clamps to epoch
/// rather than panicking.
fn now_unix() -> UnixTime {
    let since_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO);
    UnixTime::since_unix_epoch(since_epoch)
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair, PKCS_ECDSA_P256_SHA256};
    use ring::rand::SystemRandom;
    use ring::signature::{EcdsaKeyPair, ECDSA_P256_SHA256_FIXED_SIGNING};
    use time::{Duration as TimeDuration, OffsetDateTime};

    const HEADER: &str =
        r#"{"alg":"ES256","typ":"passport","ppt":"shaken","x5u":"https://c.example/c.crt"}"#;

    /// `iat` that matches `make_fixture`'s `mid_validity` (both derive from
    /// the same not_before/not_after constants), so the default fixture is
    /// `iat`-fresh when verified at `mid_validity`.
    const FRESH_IAT: i64 = 1_715_768_000;
    /// A non-disabling freshness window for the chain/sig/TN tests.
    const WINDOW: Duration = Duration::from_secs(60);

    /// A throwaway STI-PA-style CA + leaf and a PASSporT signed by the leaf,
    /// mirroring the `sip-identity` chain-test fixture so we exercise the
    /// real crypto path through `build_result`.
    struct Fixture {
        ca_der: Vec<u8>,
        leaf_der: Vec<u8>,
        passport: Passport,
        mid_validity: UnixTime,
    }

    fn payload(attest: &str, orig: &str, dest: &str) -> String {
        payload_with_iat(attest, orig, dest, FRESH_IAT)
    }

    fn payload_with_iat(attest: &str, orig: &str, dest: &str, iat: i64) -> String {
        format!(
            r#"{{"attest":"{attest}","dest":{{"tn":["{dest}"]}},"iat":{iat},"orig":{{"tn":"{orig}"}},"origid":"123e4567-e89b-12d3-a456-426655440000"}}"#
        )
    }

    fn make_fixture(payload_json: &str) -> Fixture {
        let not_before = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
        let not_after = not_before + TimeDuration::days(365);

        let ca_key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
        let mut ca_params = CertificateParams::new(vec!["Test STI-PA Root".into()]).unwrap();
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca_params.not_before = not_before;
        ca_params.not_after = not_after;
        let ca = ca_params.self_signed(&ca_key).unwrap();

        let leaf_key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
        let mut leaf_params = CertificateParams::new(vec!["sti.example.com".into()]).unwrap();
        leaf_params.not_before = not_before;
        leaf_params.not_after = not_after;
        let leaf = leaf_params.signed_by(&leaf_key, &ca, &ca_key).unwrap();

        let pkcs8 = leaf_key.serialize_der();
        let rng = SystemRandom::new();
        let signer =
            EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &pkcs8, &rng).unwrap();
        let signing_input = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(HEADER.as_bytes()),
            URL_SAFE_NO_PAD.encode(payload_json.as_bytes())
        );
        let sig = signer.sign(&rng, signing_input.as_bytes()).unwrap();
        let token = format!("{signing_input}.{}", URL_SAFE_NO_PAD.encode(sig.as_ref()));

        let mid = (not_before.unix_timestamp() + not_after.unix_timestamp()) / 2;
        Fixture {
            ca_der: ca.der().to_vec(),
            leaf_der: leaf.der().to_vec(),
            passport: Passport::decode(&token).unwrap(),
            mid_validity: UnixTime::since_unix_epoch(Duration::from_secs(mid as u64)),
        }
    }

    #[test]
    fn fully_valid_call_passes() {
        let f = make_fixture(&payload("A", "+12155551212", "+12155551213"));
        let r = build_result(
            &f.passport,
            "+12155551212",
            "+12155551213",
            std::slice::from_ref(&f.leaf_der),
            std::slice::from_ref(&f.ca_der),
            f.mid_validity,
            WINDOW,
        );
        assert!(r.passed(), "expected pass, got {r:?}");
        assert!(r.cert_chain_valid && r.signature_valid && r.orig_passed && r.dest_passed);
        assert_eq!(r.trusted_attestation(), Some(AttestationLevel::A));
        assert_eq!(r.error, None);
    }

    #[test]
    fn tn_matching_ignores_plus_and_separators() {
        let f = make_fixture(&payload("A", "+12155551212", "+12155551213"));
        // SIP From/To without the leading '+' and with separators still bind.
        let r = build_result(
            &f.passport,
            "12155551212",
            "1-215-555-1213",
            std::slice::from_ref(&f.leaf_der),
            std::slice::from_ref(&f.ca_der),
            f.mid_validity,
            WINDOW,
        );
        assert!(r.orig_passed && r.dest_passed && r.passed());
    }

    #[test]
    fn untrusted_anchor_fails_chain() {
        let f = make_fixture(&payload("A", "+12155551212", "+12155551213"));
        let other = make_fixture(&payload("A", "+12155551212", "+12155551213"));
        let r = build_result(
            &f.passport,
            "+12155551212",
            "+12155551213",
            std::slice::from_ref(&f.leaf_der),
            std::slice::from_ref(&other.ca_der), // wrong anchor
            f.mid_validity,
            WINDOW,
        );
        assert!(!r.cert_chain_valid && !r.signature_valid && !r.passed());
        assert!(r.error.unwrap().contains("chain"));
    }

    #[test]
    fn expired_chain_fails() {
        let f = make_fixture(&payload("A", "+12155551212", "+12155551213"));
        let way_future = UnixTime::since_unix_epoch(Duration::from_secs(3_000_000_000));
        let r = build_result(
            &f.passport,
            "+12155551212",
            "+12155551213",
            std::slice::from_ref(&f.leaf_der),
            std::slice::from_ref(&f.ca_der),
            way_future,
            WINDOW,
        );
        assert!(!r.cert_chain_valid && !r.passed());
    }

    #[test]
    fn orig_mismatch_fails_with_chain_still_valid() {
        let f = make_fixture(&payload("A", "+12155551212", "+12155551213"));
        let r = build_result(
            &f.passport,
            "+19998887777", // From doesn't match orig.tn
            "+12155551213",
            std::slice::from_ref(&f.leaf_der),
            std::slice::from_ref(&f.ca_der),
            f.mid_validity,
            WINDOW,
        );
        // Crypto valid, but the TN binding failed → not passed.
        assert!(r.cert_chain_valid && r.signature_valid);
        assert!(!r.orig_passed && r.dest_passed);
        assert!(!r.passed());
        assert_eq!(r.trusted_attestation(), None);
        assert_eq!(r.error.as_deref(), Some("orig.tn did not match SIP From"));
    }

    #[test]
    fn dest_mismatch_reported() {
        let f = make_fixture(&payload("B", "+12155551212", "+12155551213"));
        let r = build_result(
            &f.passport,
            "+12155551212",
            "+15005550000", // To doesn't match dest.tn
            std::slice::from_ref(&f.leaf_der),
            std::slice::from_ref(&f.ca_der),
            f.mid_validity,
            WINDOW,
        );
        assert!(r.orig_passed && !r.dest_passed && !r.passed());
        assert_eq!(r.error.as_deref(), Some("dest.tn did not match SIP To"));
    }

    #[test]
    fn tampered_payload_fails_signature_chain_valid() {
        let f = make_fixture(&payload("A", "+12155551212", "+12155551213"));
        // Forge a passport: valid leaf cert, but the signature won't match a
        // mutated payload. Re-decode a token with a swapped attest claim.
        let forged_payload = payload("C", "+12155551212", "+12155551213");
        let (hdr_b64, rest) = {
            // Reconstruct the original token's segments via signing_input.
            let si = String::from_utf8(f.passport.signing_input.clone()).unwrap();
            let (h, _p) = si.split_once('.').unwrap();
            (h.to_string(), URL_SAFE_NO_PAD.encode(&f.passport.signature))
        };
        let forged = format!(
            "{hdr_b64}.{}.{rest}",
            URL_SAFE_NO_PAD.encode(forged_payload.as_bytes())
        );
        let forged_pp = Passport::decode(&forged).unwrap();
        let r = build_result(
            &forged_pp,
            "+12155551212",
            "+12155551213",
            std::slice::from_ref(&f.leaf_der),
            std::slice::from_ref(&f.ca_der),
            f.mid_validity,
            WINDOW,
        );
        assert!(r.cert_chain_valid && !r.signature_valid && !r.passed());
        assert_eq!(
            r.error.as_deref(),
            Some("PASSporT signature did not verify")
        );
    }

    #[test]
    fn empty_chain_fails_cleanly() {
        let f = make_fixture(&payload("A", "+12155551212", "+12155551213"));
        let r = build_result(
            &f.passport,
            "+12155551212",
            "+12155551213",
            &[],
            std::slice::from_ref(&f.ca_der),
            f.mid_validity,
            WINDOW,
        );
        assert!(!r.cert_chain_valid && !r.signature_valid);
        assert!(r.error.unwrap().contains("no certificates"));
    }

    #[test]
    fn parse_failure_is_distinct_from_unsigned() {
        let r = parse_failure(IdentityError::EmptyToken);
        assert!(!r.passed());
        assert!(r.error.is_some());
        // unsigned() has no error; a malformed header does.
        assert_eq!(VerificationResult::unsigned().error, None);
    }

    #[test]
    fn attestation_levels_map_across_crates() {
        assert_eq!(
            map_attest(sip_identity::AttestationLevel::A),
            AttestationLevel::A
        );
        assert_eq!(
            map_attest(sip_identity::AttestationLevel::B),
            AttestationLevel::B
        );
        assert_eq!(
            map_attest(sip_identity::AttestationLevel::C),
            AttestationLevel::C
        );
    }

    // ─── iat freshness ───────────────────────────────────────────

    #[test]
    fn iat_fresh_helper_window_and_disable() {
        let now = UnixTime::since_unix_epoch(Duration::from_secs(1000));
        let w = Duration::from_secs(60);
        assert!(iat_fresh(Some(1000), now, w)); // exact
        assert!(iat_fresh(Some(1060), now, w)); // +60 boundary
        assert!(iat_fresh(Some(940), now, w)); // -60 boundary
        assert!(!iat_fresh(Some(1061), now, w)); // future skew > window
        assert!(!iat_fresh(Some(939), now, w)); // past skew > window
        assert!(!iat_fresh(None, now, w)); // missing iat is never fresh
        assert!(iat_fresh(None, now, Duration::ZERO)); // window 0 disables
        assert!(iat_fresh(Some(1), now, Duration::ZERO));
    }

    /// A fully-valid call whose only failure is a stale `iat`: crypto + TN
    /// all hold, but `iat_passed` is false → not passed, not trusted, and
    /// the headline error names the freshness failure.
    #[test]
    fn stale_iat_rejected_even_with_valid_crypto() {
        let f = make_fixture(&payload_with_iat(
            "A",
            "+12155551212",
            "+12155551213",
            FRESH_IAT - 3600, // an hour stale vs mid_validity
        ));
        let r = build_result(
            &f.passport,
            "+12155551212",
            "+12155551213",
            std::slice::from_ref(&f.leaf_der),
            std::slice::from_ref(&f.ca_der),
            f.mid_validity,
            WINDOW,
        );
        assert!(r.cert_chain_valid && r.signature_valid && r.orig_passed && r.dest_passed);
        assert!(!r.iat_passed && !r.passed());
        assert_eq!(r.trusted_attestation(), None);
        assert_eq!(
            r.error.as_deref(),
            Some("PASSporT iat outside the freshness window")
        );
    }

    #[test]
    fn future_iat_rejected() {
        let f = make_fixture(&payload_with_iat(
            "A",
            "+12155551212",
            "+12155551213",
            FRESH_IAT + 3600, // an hour in the future
        ));
        let r = build_result(
            &f.passport,
            "+12155551212",
            "+12155551213",
            std::slice::from_ref(&f.leaf_der),
            std::slice::from_ref(&f.ca_der),
            f.mid_validity,
            WINDOW,
        );
        assert!(!r.iat_passed && !r.passed());
    }

    #[test]
    fn missing_iat_rejected() {
        // PASSporT with no `iat` claim at all.
        let pl = r#"{"attest":"A","dest":{"tn":["+12155551213"]},"orig":{"tn":"+12155551212"}}"#;
        let f = make_fixture(pl);
        let r = build_result(
            &f.passport,
            "+12155551212",
            "+12155551213",
            std::slice::from_ref(&f.leaf_der),
            std::slice::from_ref(&f.ca_der),
            f.mid_validity,
            WINDOW,
        );
        assert!(!r.iat_passed && !r.passed());
        assert_eq!(r.error.as_deref(), Some("PASSporT has no iat claim"));
    }

    #[test]
    fn zero_window_disables_iat_check() {
        // A wildly stale iat still passes when the window is disabled.
        let f = make_fixture(&payload_with_iat(
            "A",
            "+12155551212",
            "+12155551213",
            FRESH_IAT - 100_000,
        ));
        let r = build_result(
            &f.passport,
            "+12155551212",
            "+12155551213",
            std::slice::from_ref(&f.leaf_der),
            std::slice::from_ref(&f.ca_der),
            f.mid_validity,
            Duration::ZERO,
        );
        assert!(r.iat_passed && r.passed());
    }

    // ─── x5u TLS extra-CA ─────────────────────────────────────────

    #[tokio::test]
    async fn new_accepts_a_valid_x5u_extra_ca() {
        // A real DER cert (the fixture CA) is accepted as a supplemental
        // x5u-fetch root and the client builds. (Malformed cert DER isn't
        // rejected eagerly — the rustls backend defers it to the TLS
        // handshake; the operator-facing guard is the config-layer
        // existence/≥1-cert check on the PEM file.)
        let f = make_fixture(&payload("A", "+12155551212", "+12155551213"));
        let v = Verifier::new(
            Vec::new(),
            Duration::from_secs(3600),
            Duration::from_secs(60),
            vec![f.ca_der.clone()],
        );
        assert!(v.is_ok());
    }
}
