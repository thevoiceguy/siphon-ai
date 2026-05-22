//! Shared SDP fixtures for the `siphon-ai-media-glue` integration
//! tests.
//!
//! `LINPHONE_PCMU_OFFER` and `G729_ONLY_OFFER` were duplicated
//! verbatim between `setup.rs` and `sdp_negotiation.rs`. Centralizing
//! them keeps the offer→answer negotiation tests and the end-to-end
//! `MediaSetup` tests exercising byte-identical inputs.
//!
//! Pulled in via `mod common;` per test file. `dead_code` is allowed
//! because not every test binary uses every fixture.
#![allow(dead_code)]

/// Linphone-style PCMU/PCMA offer with `telephone-event`.
pub const LINPHONE_PCMU_OFFER: &str = "v=0\r\n\
o=alice 1234 5678 IN IP4 10.0.0.5\r\n\
s=Talk\r\n\
c=IN IP4 10.0.0.5\r\n\
t=0 0\r\n\
m=audio 7078 RTP/AVP 0 8 101\r\n\
a=rtpmap:0 PCMU/8000\r\n\
a=rtpmap:8 PCMA/8000\r\n\
a=rtpmap:101 telephone-event/8000\r\n\
a=fmtp:101 0-15\r\n\
a=sendrecv\r\n";

/// G.729-only offer — no codec SiphonAI supports.
pub const G729_ONLY_OFFER: &str = "v=0\r\n\
o=- 1 1 IN IP4 10.0.0.5\r\n\
s=Talk\r\n\
c=IN IP4 10.0.0.5\r\n\
t=0 0\r\n\
m=audio 7000 RTP/AVP 18\r\n\
a=rtpmap:18 G729/8000\r\n\
a=sendrecv\r\n";
