//! The verification verdict (`verstat`) produced for an inbound call.

use serde::{Deserialize, Serialize};

use crate::attestation::AttestationLevel;

/// Outcome of STIR/SHAKEN verification for one inbound INVITE.
///
/// The booleans plus the optional error let a downstream consumer
/// reconstruct the precise failure mode without re-parsing the Identity
/// header. This is the shape surfaced on `BridgeOut::Start.verstat`, the
/// CDR, and the HEP verstat chunk.
///
/// **Trust:** `attest` is the level *claimed* in the PASSporT. It is only
/// trustworthy when the verification passed — use
/// [`VerificationResult::trusted_attestation`] for any policy decision, not
/// `attest` directly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerificationResult {
    /// Attestation level claimed in the PASSporT, if a valid one was present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attest: Option<AttestationLevel>,
    /// Originating TN from the PASSporT `orig.tn` claim.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub orig_tn: Option<String>,
    /// `orig.tn` matched the SIP `From` user.
    pub orig_passed: bool,
    /// A `dest.tn` matched the SIP `To`/R-URI user.
    pub dest_passed: bool,
    /// The signing certificate chained to a configured STI-PA trust anchor.
    pub cert_chain_valid: bool,
    /// The ES256 signature over the PASSporT verified against that cert.
    pub signature_valid: bool,
    /// The PASSporT `iat` is within the configured freshness window (replay
    /// protection, ATIS-1000074). `#[serde(default)]` so an older verstat
    /// JSON without the field deserializes to `false` rather than erroring —
    /// the field is additive (added in 0.4.1).
    #[serde(default)]
    pub iat_passed: bool,
    /// Human-readable failure reason when verification did not fully pass.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl VerificationResult {
    /// A verdict for a call with no `Identity` header at all — nothing
    /// verified, no attestation. Distinct from a present-but-invalid header
    /// (which carries an `error`).
    pub fn unsigned() -> Self {
        Self {
            attest: None,
            orig_tn: None,
            orig_passed: false,
            dest_passed: false,
            cert_chain_valid: false,
            signature_valid: false,
            iat_passed: false,
            error: None,
        }
    }

    /// Composite pass: every check succeeded. This is the `verstat_passed`
    /// CDR field and the precondition for trusting `attest`.
    pub fn passed(&self) -> bool {
        self.signature_valid
            && self.cert_chain_valid
            && self.orig_passed
            && self.dest_passed
            && self.iat_passed
    }

    /// The attestation level we are willing to *trust* — the claimed level
    /// only when [`passed`](Self::passed) holds, else `None`. Policy gates
    /// must use this, never the raw `attest` claim, so an unsigned or
    /// invalid call can never satisfy an attestation requirement.
    pub fn trusted_attestation(&self) -> Option<AttestationLevel> {
        if self.passed() {
            self.attest
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn good(attest: AttestationLevel) -> VerificationResult {
        VerificationResult {
            attest: Some(attest),
            orig_tn: Some("+12155551212".into()),
            orig_passed: true,
            dest_passed: true,
            cert_chain_valid: true,
            signature_valid: true,
            iat_passed: true,
            error: None,
        }
    }

    #[test]
    fn passed_requires_all_checks() {
        assert!(good(AttestationLevel::A).passed());
        let mut r = good(AttestationLevel::A);
        r.signature_valid = false;
        assert!(!r.passed());
    }

    #[test]
    fn stale_iat_fails_the_composite() {
        // A cryptographically-valid call with a stale iat is not passed —
        // and so yields no trusted attestation (replay protection).
        let mut r = good(AttestationLevel::A);
        r.iat_passed = false;
        assert!(!r.passed());
        assert_eq!(r.trusted_attestation(), None);
    }

    #[test]
    fn trusted_attestation_gated_on_pass() {
        assert_eq!(
            good(AttestationLevel::B).trusted_attestation(),
            Some(AttestationLevel::B)
        );
        // Claimed A but signature failed → not trusted.
        let mut r = good(AttestationLevel::A);
        r.signature_valid = false;
        assert_eq!(r.trusted_attestation(), None);
    }

    #[test]
    fn unsigned_has_no_trust() {
        let u = VerificationResult::unsigned();
        assert!(!u.passed());
        assert_eq!(u.trusted_attestation(), None);
        assert_eq!(u.error, None);
    }

    #[test]
    fn serializes_without_null_optionals() {
        let json = serde_json::to_string(&VerificationResult::unsigned()).unwrap();
        // attest/orig_tn/error skipped when None; booleans always present.
        assert!(!json.contains("attest"));
        assert!(!json.contains("error"));
        assert!(json.contains("\"signature_valid\":false"));
    }
}
