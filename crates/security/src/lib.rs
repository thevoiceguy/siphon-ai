//! Call-authentication types for SiphonAI (STIR/SHAKEN).
//!
//! This crate owns the **typed vocabulary** of the 0.4.0 call-authentication
//! theme — the verstat verdict, attestation levels, the minimum-attestation
//! policy gate, and the compiled `[security.stir_shaken]` config — without
//! any of the verification machinery. It's deliberately small and
//! dependency-light so every other layer (config, core accept path,
//! bridge protocol, CDR, HEP) can depend on it cheaply.
//!
//! ## What lives here
//!
//! - [`AttestationLevel`] — SHAKEN A/B/C, with an explicit trust [`rank`].
//! - [`VerificationResult`] — the per-call verstat verdict surfaced on
//!   `BridgeOut::Start`, the CDR, and HEP. Use
//!   [`trusted_attestation`] for policy, never the raw `attest` claim.
//! - [`MinAttestation`] — the policy gate: parse, strict per-route
//!   [`resolve`], and [`permits`] implementing the §4 matrix.
//! - [`StirShakenConfig`] + [`validate_trust_anchors`] — compiled config and
//!   load-time trust-anchor plumbing.
//!
//! ## What does NOT live here (yet)
//!
//! The verifier: Identity-header parsing (that's siphon-rs `sip-identity`),
//! ES256 signature checking, `x5u` cert fetch, and chain validation against
//! the trust anchors. Those wire a [`VerificationResult`] *in*; this crate
//! only defines its shape and what policy does with it.
//!
//! [`rank`]: AttestationLevel::rank
//! [`trusted_attestation`]: VerificationResult::trusted_attestation
//! [`resolve`]: MinAttestation::resolve
//! [`permits`]: MinAttestation::permits

mod attestation;
mod config;
mod policy;
mod verstat;

pub use attestation::AttestationLevel;
pub use config::{
    validate_trust_anchors, StirShakenConfig, TrustAnchorError, DEFAULT_CERT_CACHE_TTL,
    DEFAULT_IAT_FRESHNESS,
};
pub use policy::MinAttestation;
pub use verstat::VerificationResult;
