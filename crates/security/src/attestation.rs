//! SHAKEN attestation level (ATIS-1000074 §5.2.3).

use serde::{Deserialize, Serialize};

/// How strongly the originating provider vouches for the calling number.
///
/// Ordered by trust strength: `A` (full) is the strongest, `C` (gateway)
/// the weakest. Use [`AttestationLevel::rank`] for policy comparisons
/// rather than relying on a derived ordering — the wire characters (`A`
/// highest) run opposite to alphabetical intuition, so the rank is made
/// explicit on purpose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AttestationLevel {
    /// Full attestation — provider authenticated the customer and confirmed
    /// their right to use the calling number.
    #[serde(rename = "A")]
    A,
    /// Partial attestation — origin authenticated, number authorization not
    /// confirmed.
    #[serde(rename = "B")]
    B,
    /// Gateway attestation — only the ingress point is authenticated.
    #[serde(rename = "C")]
    C,
}

impl AttestationLevel {
    /// Trust strength: `A`=3 (strongest) … `C`=1 (weakest). A call satisfies
    /// a minimum-attestation policy when its rank is `>=` the policy's rank.
    pub fn rank(self) -> u8 {
        match self {
            Self::A => 3,
            Self::B => 2,
            Self::C => 1,
        }
    }

    /// Parse the single-character claim value (`"A"` / `"B"` / `"C"`).
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "A" => Some(Self::A),
            "B" => Some(Self::B),
            "C" => Some(Self::C),
            _ => None,
        }
    }

    /// The wire character.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::A => "A",
            Self::B => "B",
            Self::C => "C",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rank_orders_a_strongest() {
        assert!(AttestationLevel::A.rank() > AttestationLevel::B.rank());
        assert!(AttestationLevel::B.rank() > AttestationLevel::C.rank());
    }

    #[test]
    fn parse_roundtrips() {
        for s in ["A", "B", "C"] {
            assert_eq!(AttestationLevel::parse(s).unwrap().as_str(), s);
        }
        assert_eq!(AttestationLevel::parse("D"), None);
    }

    #[test]
    fn serde_uses_wire_chars() {
        let json = serde_json::to_string(&AttestationLevel::A).unwrap();
        assert_eq!(json, "\"A\"");
    }
}
