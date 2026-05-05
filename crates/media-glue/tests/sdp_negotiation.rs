//! End-to-end offer → answer tests.
//!
//! Offers are written verbatim from realistic softphone traces
//! (Linphone, X-Lite, Asterisk-originated INVITEs) so the
//! negotiator has to deal with real-world SDP, not tidy
//! synthesized fixtures.

use forge_sdp::{MediaType, SessionDescription, SessionDescriptionExt};
use siphon_ai_media_glue::{build_answer, parse_offer, Codec, LocalCapabilities, SdpError};

fn caps_pcmu_only(port: u16) -> LocalCapabilities {
    LocalCapabilities {
        local_ip: "192.168.1.10".into(),
        local_port: port,
        codecs: vec![Codec::Pcmu],
        dtmf_payload_type: None,
    }
}

fn caps_full(port: u16) -> LocalCapabilities {
    LocalCapabilities {
        local_ip: "192.168.1.10".into(),
        local_port: port,
        codecs: vec![Codec::Opus, Codec::G722, Codec::Pcmu, Codec::Pcma],
        dtmf_payload_type: Some(101),
    }
}

const LINPHONE_PCMU_OFFER: &str = "v=0\r\n\
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

const ASTERISK_OPUS_PREF_OFFER: &str = "v=0\r\n\
o=- 1700000000 1 IN IP4 10.0.0.5\r\n\
s=Asterisk\r\n\
c=IN IP4 10.0.0.5\r\n\
t=0 0\r\n\
m=audio 18000 RTP/AVP 111 9 0 8 101\r\n\
a=rtpmap:111 opus/48000/2\r\n\
a=rtpmap:9 G722/8000\r\n\
a=rtpmap:0 PCMU/8000\r\n\
a=rtpmap:8 PCMA/8000\r\n\
a=rtpmap:101 telephone-event/8000\r\n\
a=fmtp:101 0-16\r\n\
a=sendrecv\r\n";

const VIDEO_ONLY_OFFER: &str = "v=0\r\n\
o=- 1 1 IN IP4 10.0.0.5\r\n\
s=Video\r\n\
c=IN IP4 10.0.0.5\r\n\
t=0 0\r\n\
m=video 5004 RTP/AVP 96\r\n\
a=rtpmap:96 H264/90000\r\n\
a=sendrecv\r\n";

const G729_ONLY_OFFER: &str = "v=0\r\n\
o=- 1 1 IN IP4 10.0.0.5\r\n\
s=Talk\r\n\
c=IN IP4 10.0.0.5\r\n\
t=0 0\r\n\
m=audio 7000 RTP/AVP 18\r\n\
a=rtpmap:18 G729/8000\r\n\
a=sendrecv\r\n";

#[test]
fn pcmu_only_offer_yields_pcmu_answer_at_our_port() {
    let answer = build_answer(LINPHONE_PCMU_OFFER, &caps_pcmu_only(20100)).unwrap();
    assert_eq!(answer.negotiated_codec, Codec::Pcmu);
    assert_eq!(answer.negotiated_payload_type, 0);
    assert_eq!(answer.negotiated_clock_rate, 8000);
    assert_eq!(answer.negotiated_audio_sample_rate, 8000);

    // The serialized answer must reflect our local IP and port —
    // that's what makes RTP actually flow back to us.
    assert!(answer.answer_text.contains("c=IN IP4 192.168.1.10"));
    assert!(answer.answer_text.contains("m=audio 20100 RTP/AVP"));
    // PCMU rtpmap appears.
    assert!(answer.answer_text.contains("a=rtpmap:0 PCMU/8000"));
    // We didn't enable telephone-event in caps, so it shouldn't
    // appear in the answer's formats list (the offer's PT 101 is
    // dropped because we don't advertise it locally).
    assert!(!answer.answer_text.contains("a=rtpmap:101"));
}

#[test]
fn multi_codec_offer_picks_offer_order_subject_to_our_caps() {
    // sip-sdp's negotiator iterates the offer's formats and picks
    // the first one we also support. Asterisk's offer leads with
    // opus → opus wins (we have it in caps).
    let answer = build_answer(ASTERISK_OPUS_PREF_OFFER, &caps_full(20100)).unwrap();
    assert_eq!(answer.negotiated_codec, Codec::Opus);
    assert_eq!(answer.negotiated_clock_rate, 48000);
    assert_eq!(answer.negotiated_audio_sample_rate, 48000);

    // The answer should still echo the offer's PT (111), not our
    // canonical 111. They happen to match here, but for dynamic
    // PTs the offerer's number wins per RFC 3264 §6.1.
    assert_eq!(answer.negotiated_payload_type, 111);
}

#[test]
fn dtmf_advertised_when_caps_request_it() {
    let answer = build_answer(LINPHONE_PCMU_OFFER, &caps_full(20100)).unwrap();
    // PCMU still wins (it's the first format both sides support).
    assert_eq!(answer.negotiated_codec, Codec::Pcmu);
    // Telephone-event survives because both sides offered/advertise PT 101.
    assert!(answer
        .answer_text
        .contains("a=rtpmap:101 telephone-event/8000"));
}

#[test]
fn video_only_offer_yields_no_audio() {
    let result = build_answer(VIDEO_ONLY_OFFER, &caps_full(20100));
    let err = result.unwrap_err();
    // The negotiator either rejects the audio (port 0) or the
    // upstream `find_media(Audio)` returns None — either way we
    // surface a "no audio" condition.
    assert!(matches!(err, SdpError::NoAudio | SdpError::Negotiate(_)));
}

#[test]
fn no_common_audio_codec_yields_no_common_codec() {
    let result = build_answer(G729_ONLY_OFFER, &caps_full(20100));
    let err = result.unwrap_err();
    // sip-sdp's negotiate returns a rejected media stream (port 0)
    // with no formats; our wrapper surfaces NoCommonCodec.
    assert!(
        matches!(err, SdpError::NoCommonCodec | SdpError::AudioRejected),
        "expected NoCommonCodec / AudioRejected, got {err:?}"
    );
}

#[test]
fn malformed_offer_yields_parse_error() {
    let result = build_answer("not actually sdp", &caps_full(20100));
    assert!(matches!(result.unwrap_err(), SdpError::Parse(_)));
}

#[test]
fn answer_is_well_formed_sdp() {
    let answer = build_answer(LINPHONE_PCMU_OFFER, &caps_full(20100)).unwrap();
    // Round-trip: parse the serialized answer back through
    // forge-sdp's parser. If we generated something the parser
    // can't accept, our consumer (the SIP UAS) would reject it
    // before sending the 200 OK.
    let reparsed = SessionDescription::from_str(&answer.answer_text).expect("reparse");
    let audio = reparsed
        .find_media(MediaType::Audio)
        .expect("audio in answer");
    assert_eq!(audio.port, 20100);
}

#[test]
fn parse_offer_separates_parsing_from_negotiation() {
    // Useful for callers that want to inspect the offer (codec
    // list, direction, c= line) before deciding whether to answer
    // or reject. Our wrapper lets them do that without holding
    // the full negotiator in their head.
    let offer = parse_offer(LINPHONE_PCMU_OFFER).unwrap();
    let audio = offer.find_media(MediaType::Audio).expect("audio in offer");
    assert_eq!(audio.port, 7078);
    // PCMU and PCMA offered.
    assert!(audio.formats.iter().any(|f| f == "0"));
    assert!(audio.formats.iter().any(|f| f == "8"));
}

#[test]
fn answer_port_can_differ_from_offer_port() {
    // Trivial guarantee but worth pinning: the answer's port comes
    // from `caps.local_port`, never the offer's.
    let answer = build_answer(LINPHONE_PCMU_OFFER, &caps_pcmu_only(31337)).unwrap();
    assert!(answer.answer_text.contains("m=audio 31337 RTP/AVP"));
}
