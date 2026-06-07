//! Minimum-attestation policy: the gate that decides whether a verified
//! call is allowed through, and how global/per-route policies combine.

use crate::attestation::AttestationLevel;
use crate::verstat::VerificationResult;

/// The minimum attestation a call must carry to be accepted.
///
/// `None` admits everything (the default — zero behaviour change for
/// deployments that don't opt in). `A`/`B`/`C` require a *trusted*
/// attestation at or above that level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MinAttestation {
    /// No attestation requirement — every call passes the gate.
    #[default]
    None,
    /// Require trusted attestation `C` or better.
    C,
    /// Require trusted attestation `B` or better.
    B,
    /// Require trusted attestation `A`.
    A,
}

impl MinAttestation {
    /// Parse the config value. `"none"` (or unset) → [`MinAttestation::None`].
    /// Case-insensitive on the level letters. Returns `None` for an
    /// unrecognised value so the caller can surface a precise config error.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "none" => Some(Self::None),
            "a" => Some(Self::A),
            "b" => Some(Self::B),
            "c" => Some(Self::C),
            _ => None,
        }
    }

    /// The config/wire token: `"none"` / `"A"` / `"B"` / `"C"`. The inverse
    /// of [`parse`](Self::parse), for logs and diagnostics.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::C => "C",
            Self::B => "B",
            Self::A => "A",
        }
    }

    /// The required rank, or `None` when there is no requirement. Mirrors
    /// [`AttestationLevel::rank`] so the two compare directly.
    fn required_rank(self) -> Option<u8> {
        match self {
            Self::None => None,
            Self::C => Some(AttestationLevel::C.rank()),
            Self::B => Some(AttestationLevel::B.rank()),
            Self::A => Some(AttestationLevel::A.rank()),
        }
    }

    /// Resolve the effective policy for a route. **Strict override**: a
    /// route value fully replaces the global, even if more permissive
    /// (matching `[route.bridge.tls]` semantics from 0.3.0). A route with
    /// no override inherits the global.
    pub fn resolve(
        global: MinAttestation,
        route_override: Option<MinAttestation>,
    ) -> MinAttestation {
        route_override.unwrap_or(global)
    }

    /// Decide whether a call's verification verdict satisfies this policy.
    ///
    /// Implements the §4 policy matrix: `None` always admits; otherwise the
    /// call needs a *trusted* attestation (verification fully passed) at or
    /// above the required rank. An unsigned call, an invalid signature, or a
    /// header below the threshold is rejected.
    pub fn permits(self, result: &VerificationResult) -> bool {
        match self.required_rank() {
            None => true,
            Some(required) => result
                .trusted_attestation()
                .is_some_and(|level| level.rank() >= required),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn verdict(attest: Option<AttestationLevel>, passed: bool) -> VerificationResult {
        VerificationResult {
            attest,
            orig_tn: None,
            orig_passed: passed,
            dest_passed: passed,
            cert_chain_valid: passed,
            signature_valid: passed,
            error: None,
        }
    }

    #[test]
    fn parse_accepts_none_and_levels_case_insensitively() {
        assert_eq!(MinAttestation::parse("none"), Some(MinAttestation::None));
        assert_eq!(MinAttestation::parse("A"), Some(MinAttestation::A));
        assert_eq!(MinAttestation::parse("b"), Some(MinAttestation::B));
        assert_eq!(MinAttestation::parse(" C "), Some(MinAttestation::C));
        assert_eq!(MinAttestation::parse("strong"), None);
    }

    #[test]
    fn default_is_none() {
        assert_eq!(MinAttestation::default(), MinAttestation::None);
    }

    #[test]
    fn as_str_inverts_parse() {
        for m in [
            MinAttestation::None,
            MinAttestation::A,
            MinAttestation::B,
            MinAttestation::C,
        ] {
            assert_eq!(MinAttestation::parse(m.as_str()), Some(m));
        }
        assert_eq!(MinAttestation::None.as_str(), "none");
    }

    #[test]
    fn strict_override_can_loosen_or_tighten() {
        // Route override fully replaces global, even when more permissive.
        assert_eq!(
            MinAttestation::resolve(MinAttestation::B, Some(MinAttestation::C)),
            MinAttestation::C
        );
        assert_eq!(
            MinAttestation::resolve(MinAttestation::B, Some(MinAttestation::A)),
            MinAttestation::A
        );
        // No override → inherit global.
        assert_eq!(
            MinAttestation::resolve(MinAttestation::B, None),
            MinAttestation::B
        );
    }

    /// The full §4 policy matrix.
    #[test]
    fn policy_matrix() {
        let a = verdict(Some(AttestationLevel::A), true);
        let b = verdict(Some(AttestationLevel::B), true);
        let c = verdict(Some(AttestationLevel::C), true);
        let absent = verdict(None, false);
        let bad_sig = verdict(Some(AttestationLevel::A), false); // claims A, failed verify

        // none: everything passes.
        for v in [&a, &b, &c, &absent, &bad_sig] {
            assert!(MinAttestation::None.permits(v));
        }
        // C: A,B,C pass; absent + invalid reject.
        assert!(MinAttestation::C.permits(&a));
        assert!(MinAttestation::C.permits(&b));
        assert!(MinAttestation::C.permits(&c));
        assert!(!MinAttestation::C.permits(&absent));
        assert!(!MinAttestation::C.permits(&bad_sig));
        // B: A,B pass; C,absent,invalid reject.
        assert!(MinAttestation::B.permits(&a));
        assert!(MinAttestation::B.permits(&b));
        assert!(!MinAttestation::B.permits(&c));
        assert!(!MinAttestation::B.permits(&absent));
        // A: only A passes.
        assert!(MinAttestation::A.permits(&a));
        assert!(!MinAttestation::A.permits(&b));
        assert!(!MinAttestation::A.permits(&c));
    }
}
