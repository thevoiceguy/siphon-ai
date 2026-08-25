//! Is this offer WebRTC-shaped?
//!
//! The plan's leg-selection rule (`DEV_PLAN_WebRTC.md` §4.1) is
//! **transport type selects eligibility, SDP shape selects the leg**:
//! an INVITE over WS/WSS *may* be a browser, and only the offer says
//! whether it is. A non-browser RFC 7118 client sending a plain
//! `RTP/AVP` offer still gets a classic RTP leg — no magic.
//!
//! This module owns the second half of that rule. It is a pure
//! inspection of the parsed offer, so it is cheap, has no side
//! effects, and agrees with the classic negotiator by construction
//! (same `forge_sdp` types).
//!
//! # What actually makes an offer "WebRTC"
//!
//! RFC 8829 (JSEP) leaves no single marker, so the test is the
//! conjunction browsers always satisfy and classic SIP peers never do:
//!
//! 1. an audio m-line on a **DTLS profile** (`UDP/TLS/RTP/SAVPF`, or
//!    its rare `TCP/TLS/...` cousin), and
//! 2. **ICE credentials** (`a=ice-ufrag` + `a=ice-pwd`), and
//! 3. a **DTLS fingerprint** (`a=fingerprint`).
//!
//! All three are mandatory in a browser offer. A SIP peer doing plain
//! DTLS-SRTP without ICE (some SBCs) satisfies 1 and 3 but not 2, and
//! must keep going down the classic path — which is why ICE, not the
//! profile, is the discriminator that matters.
//!
//! `a=group:BUNDLE` and `a=rtcp-mux` are *recorded* rather than
//! required: both are universal in practice, but neither is
//! load-bearing for the decision, and demanding them would reject a
//! future client for cosmetic reasons.

use forge_sdp::{
    IceAttributesExt, MediaDtlsAttributesExt, MediaIceAttributesExt, MediaType, Protocol,
    SessionDescription, SessionDescriptionExt,
};

/// What the offer inspection found. Returned even when the verdict is
/// "not WebRTC" so callers can log *why* a browser-looking offer was
/// not treated as one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OfferShape {
    /// Index of the first audio m-line with a non-zero port.
    pub audio_index: Option<usize>,
    /// The audio m-line sits on `UDP/TLS/RTP/SAVPF` (or `TCP/TLS/…`).
    pub dtls_profile: bool,
    /// `a=ice-ufrag` **and** `a=ice-pwd` are present (media level,
    /// falling back to session level per RFC 8839 §5.4).
    pub ice_credentials: bool,
    /// `a=fingerprint` is present (media level, session fallback per
    /// RFC 8122 §5).
    pub fingerprint: bool,
    /// `a=group:BUNDLE` at session level. Informational.
    pub bundle: bool,
    /// `a=rtcp-mux` on the audio m-line. Informational.
    pub rtcp_mux: bool,
    /// The remote listed at least one `a=candidate` inline. Browsers
    /// trickle, so an initial offer may legitimately carry none.
    pub inline_candidates: usize,
    /// Host candidates whose address is an mDNS name (`*.local`).
    /// Chrome obfuscates host candidates this way by default; a
    /// server with no mDNS resolver cannot use them and must rely on
    /// the peer's server-reflexive candidate (or its own, if it is
    /// the reachable side). Recorded because it explains an ICE
    /// failure that otherwise looks like a network problem.
    pub mdns_candidates: usize,
}

impl OfferShape {
    /// The plan's §4.1 verdict: DTLS profile **and** ICE credentials
    /// **and** a fingerprint.
    pub fn is_webrtc(&self) -> bool {
        self.dtls_profile && self.ice_credentials && self.fingerprint
    }

    /// Why this offer is not WebRTC-shaped, for logs. `None` when it
    /// is.
    pub fn why_not(&self) -> Option<&'static str> {
        if self.audio_index.is_none() {
            return Some("no audio m-line with a non-zero port");
        }
        if !self.dtls_profile {
            return Some("audio m-line is not on a DTLS-SRTP profile");
        }
        if !self.ice_credentials {
            return Some("no ICE credentials (a=ice-ufrag / a=ice-pwd)");
        }
        if !self.fingerprint {
            return Some("no DTLS fingerprint (a=fingerprint)");
        }
        None
    }
}

/// Inspect a parsed offer. See [`OfferShape`].
pub fn inspect(offer: &SessionDescription) -> OfferShape {
    let audio_index = offer
        .media
        .iter()
        .position(|m| m.media_type == MediaType::Audio && m.port != 0);

    let Some(idx) = audio_index else {
        return OfferShape {
            audio_index: None,
            dtls_profile: false,
            ice_credentials: false,
            fingerprint: false,
            bundle: session_has_bundle(offer),
            rtcp_mux: false,
            inline_candidates: 0,
            mdns_candidates: 0,
        };
    };
    let m = &offer.media[idx];

    // Media level first (what browsers emit), session level as the
    // RFC-sanctioned fallback (RFC 8839 §5.4, RFC 8122 §5).
    let ice_credentials = m
        .get_media_ice_credentials()
        .or_else(|| offer.get_ice_credentials())
        .is_some_and(|(ufrag, pwd)| !ufrag.is_empty() && !pwd.is_empty());
    let fingerprint = m.get_media_dtls_fingerprint().is_some()
        || <SessionDescription as forge_sdp::DtlsAttributesExt>::get_dtls_fingerprint(offer)
            .is_some();

    let candidates = MediaIceAttributesExt::get_ice_candidates(m);
    let mdns_candidates = candidates
        .iter()
        .filter(|c| candidate_address(c).is_some_and(|a| a.ends_with(".local")))
        .count();

    OfferShape {
        audio_index: Some(idx),
        dtls_profile: matches!(
            m.protocol,
            Protocol::UdpTlsRtpSavpf | Protocol::TcpTlsRtpSavpf
        ),
        ice_credentials,
        fingerprint,
        bundle: session_has_bundle(offer),
        rtcp_mux: m
            .attributes
            .iter()
            .any(|a| matches!(a, forge_sdp::Attribute::Property(p) if p == "rtcp-mux")),
        inline_candidates: candidates.len(),
        mdns_candidates,
    }
}

/// Convenience: the §4.1 verdict for an offer that may not parse.
/// A body we cannot parse is not WebRTC — the classic path will
/// produce the appropriate error for it.
pub fn is_webrtc_offer(sdp: &str) -> bool {
    SessionDescription::from_str(sdp).is_ok_and(|p| inspect(&p).is_webrtc())
}

fn session_has_bundle(offer: &SessionDescription) -> bool {
    offer.attributes.iter().any(|a| {
        matches!(a, forge_sdp::Attribute::Value { name, value }
            if name == "group" && value.starts_with("BUNDLE"))
    })
}

/// The connection-address field of an `a=candidate:` line
/// (RFC 8839 §5.1): `foundation component transport priority ADDRESS
/// port typ …`.
fn candidate_address(attr: &str) -> Option<&str> {
    attr.split_whitespace().nth(4)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real thing: captured off the wire (via Homer) during the
    /// Phase 1 browser check — Chrome + SIP.js 0.21.1 calling the
    /// daemon over WSS. Kept verbatim as the fixture every detection
    /// claim is checked against.
    const CHROME_OFFER: &str = include_str!("../fixtures/chrome-offer.sdp");

    /// A classic SIP peer's offer — what every non-browser leg sends.
    const PLAIN_RTP_OFFER: &str = "v=0\r\n\
o=user1 53655765 2353687637 IN IP4 192.0.2.10\r\n\
s=-\r\n\
c=IN IP4 192.0.2.10\r\n\
t=0 0\r\n\
m=audio 9000 RTP/AVP 0\r\n\
a=rtpmap:0 PCMU/8000\r\n";

    /// An SBC doing DTLS-SRTP *without* ICE. Satisfies the profile and
    /// the fingerprint but is not a browser — must stay on the classic
    /// path, which is why ICE is the discriminator.
    const DTLS_NO_ICE_OFFER: &str = "v=0\r\n\
o=sbc 1 1 IN IP4 192.0.2.20\r\n\
s=-\r\n\
c=IN IP4 192.0.2.20\r\n\
t=0 0\r\n\
m=audio 20000 UDP/TLS/RTP/SAVPF 0\r\n\
a=rtpmap:0 PCMU/8000\r\n\
a=fingerprint:sha-256 AA:BB:CC:DD:EE:FF:00:11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF:00:11:22:33:44:55:66:77:88:99\r\n\
a=setup:actpass\r\n";

    fn parse(s: &str) -> SessionDescription {
        SessionDescription::from_str(s).expect("fixture parses")
    }

    #[test]
    fn real_chrome_offer_is_webrtc() {
        let shape = inspect(&parse(CHROME_OFFER));
        assert!(shape.is_webrtc(), "{shape:?}");
        assert_eq!(shape.why_not(), None);
        assert_eq!(shape.audio_index, Some(0));
        assert!(shape.dtls_profile && shape.ice_credentials && shape.fingerprint);
        // Universal in practice, recorded but not required.
        assert!(shape.bundle);
        assert!(shape.rtcp_mux);
    }

    /// Chrome obfuscates host candidates as mDNS `.local` names by
    /// default. A server with no mDNS resolver cannot use them — it
    /// needs the peer's srflx candidate, or must be the reachable side
    /// itself. Pinned because an ICE failure caused by this looks like
    /// a network problem unless you know to expect it.
    #[test]
    fn chrome_offer_carries_an_mdns_host_candidate() {
        let shape = inspect(&parse(CHROME_OFFER));
        assert_eq!(shape.inline_candidates, 2);
        assert_eq!(
            shape.mdns_candidates, 1,
            "Chrome's host candidate is an mDNS name; the other is srflx"
        );
    }

    #[test]
    fn plain_rtp_offer_is_not_webrtc() {
        let shape = inspect(&parse(PLAIN_RTP_OFFER));
        assert!(!shape.is_webrtc());
        assert_eq!(
            shape.why_not(),
            Some("audio m-line is not on a DTLS-SRTP profile")
        );
    }

    #[test]
    fn dtls_without_ice_is_not_webrtc() {
        let shape = inspect(&parse(DTLS_NO_ICE_OFFER));
        assert!(shape.dtls_profile && shape.fingerprint);
        assert!(!shape.is_webrtc(), "an SBC's DTLS offer is not a browser");
        assert_eq!(
            shape.why_not(),
            Some("no ICE credentials (a=ice-ufrag / a=ice-pwd)")
        );
    }

    #[test]
    fn ice_without_fingerprint_is_not_webrtc() {
        // Malformed/hostile: ICE present, fingerprint stripped. Must
        // not reach the WebRTC leg (there would be nothing to bind the
        // DTLS identity to).
        let sdp = CHROME_OFFER
            .lines()
            .filter(|l| !l.starts_with("a=fingerprint:"))
            .collect::<Vec<_>>()
            .join("\r\n");
        let shape = inspect(&parse(&sdp));
        assert!(!shape.is_webrtc());
        assert_eq!(shape.why_not(), Some("no DTLS fingerprint (a=fingerprint)"));
    }

    #[test]
    fn rejected_audio_line_is_not_webrtc() {
        // port 0 = the m-line is declined; there is no leg to build.
        let sdp = CHROME_OFFER.replace("m=audio 41827 ", "m=audio 0 ");
        let shape = inspect(&parse(&sdp));
        assert!(!shape.is_webrtc());
        assert_eq!(
            shape.why_not(),
            Some("no audio m-line with a non-zero port")
        );
    }

    #[test]
    fn unparseable_body_is_not_webrtc() {
        assert!(!is_webrtc_offer("this is not sdp"));
        assert!(!is_webrtc_offer(""));
    }

    #[test]
    fn convenience_wrapper_agrees_with_inspect() {
        assert!(is_webrtc_offer(CHROME_OFFER));
        assert!(!is_webrtc_offer(PLAIN_RTP_OFFER));
    }
}
