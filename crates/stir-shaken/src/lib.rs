//! STIR/SHAKEN verification service for SiphonAI.
//!
//! This crate is the **application-layer half** of call authentication: the
//! parts that need a runtime, the network, and a cache, which the
//! `sip-identity` stack crate deliberately leaves out. Given the raw
//! `Identity` header off an inbound INVITE, the [`Verifier`]:
//!
//! 1. parses it (RFC 8224 header + RFC 8225 PASSporT — `sip-identity`),
//! 2. fetches the signing certificate from its `x5u` over HTTPS, caching it
//!    by URL with a TTL (default 1h),
//! 3. validates the certificate chains to a configured STI-PA trust anchor
//!    and verifies the ES256 PASSporT signature under it (`sip-identity`),
//! 4. checks the SHAKEN `orig.tn`/`dest.tn` claims against the SIP `From`
//!    and `To`, and
//! 5. folds all of that into a [`VerificationResult`] — the verstat verdict
//!    the rest of the system surfaces and the [`MinAttestation`] policy
//!    gates on.
//!
//! It never decides call admission itself: it produces the verdict; the
//! accept path applies policy. The verstat *types* live in
//! `siphon-ai-security` (re-exported here for convenience); this crate
//! fills them in.
//!
//! [`VerificationResult`]: siphon_ai_security::VerificationResult
//! [`MinAttestation`]: siphon_ai_security::MinAttestation

mod cache;
mod fetch;
mod pem;
mod verifier;

pub use fetch::FetchError;
pub use pem::{load_trust_anchors, parse_cert_chain, TrustAnchorLoadError};
pub use verifier::{BuildError, Verifier};

// Re-exported so consumers can name the verdict and its vocabulary without a
// separate dependency on `siphon-ai-security`.
pub use siphon_ai_security::{AttestationLevel, StirShakenConfig, VerificationResult};
