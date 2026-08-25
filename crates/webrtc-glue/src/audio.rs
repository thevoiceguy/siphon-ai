//! The browser leg's audio path: SRTP ⇄ the bridge's PCM16LE frames
//! (`DEV_PLAN_WebRTC.md` §4.3).
//!
//! forge-webrtc hands over RTP that is authenticated, decrypted and
//! parsed — but still **codec-encoded**, with no jitter buffer and no
//! depacketization. Everything between that and the WS bridge's fixed
//! 20 ms PCM16LE frames lives here:
//!
//! ```text
//!   inbound   RtpPacket ─► JitterBuffer ─► decode ─► Reframer ─► PCM16LE bytes
//!   outbound  PCM16LE bytes ─► i16 ─► encode ─► AudioSender::send_audio
//! ```
//!
//! Two deliberate reuses keep this leg honest against the classic one:
//! the **same** `Reframer` / `pack_pcm16_le` the classic tap uses (so
//! the frame contract cannot drift between leg types), and forge-rtp's
//! **`JitterBuffer`** rather than a bespoke reorder queue (CLAUDE.md
//! §4.8 — forge already solved this for the classic path).
//!
//! # The RTP-clock trap
//!
//! Opus decodes to whatever rate we ask for (16 kHz here, matching the
//! classic path), but its **RTP timestamp always advances at 48 kHz**
//! regardless — RFC 7587 §4.1. So a 20 ms frame is 320 PCM samples and
//! 960 timestamp units. Passing the PCM length as the timestamp
//! increment would make our clock run at a third of real time and the
//! browser's jitter buffer would slowly starve. [`OutboundAudio`]
//! therefore takes the increment from forge-webrtc's
//! `AudioSender::samples_per_20ms()` and never from the PCM frame.

use bytes::Bytes;
use forge_codecs::{g711::G711ALaw, g711::G711MuLaw, AudioCodec as CodecTrait};
use forge_core::AudioCodec;
use forge_rtp::{jitter::JitterBuffer, RtpPacket};
use forge_webrtc::AudioSender;
use siphon_ai_bridge::audio::{pack_pcm16_le, unpack_pcm16_le, Reframer};
use std::time::Duration;
use tracing::{debug, warn};

use crate::{Result, WebRtcGlueError};

/// Jitter-buffer target depth. Browser paths are lossier and jitterier
/// than SIP trunks, but this leg feeds a speech pipeline, so latency is
/// as real a cost as reordering: 60 ms (three frames) absorbs ordinary
/// internet jitter while staying well inside a conversational budget.
pub const JITTER_TARGET: Duration = Duration::from_millis(60);

/// Build the codec for one direction at `rate`.
///
/// One instance per direction per call: `AudioCodec::encode`/`decode`
/// take `&mut self`, and sharing would need a lock on the audio path
/// (CLAUDE.md §4.3 forbids that).
fn codec_for(codec: AudioCodec, rate: u32) -> Result<Box<dyn CodecTrait>> {
    match codec {
        AudioCodec::PCMU => Ok(Box::new(G711MuLaw::new(rate))),
        AudioCodec::PCMA => Ok(Box::new(G711ALaw::new(rate))),
        AudioCodec::Opus => {
            #[cfg(feature = "opus")]
            {
                let cfg = forge_codecs::opus::OpusConfig {
                    // Decode straight to the bridge rate — libopus does
                    // 48↔16 and stereo→mono internally, which is how the
                    // classic Opus path already works.
                    sample_rate: rate,
                    channels: 1,
                    ..forge_codecs::opus::OpusConfig::voip()
                };
                forge_codecs::opus::OpusCodec::with_config(cfg)
                    .map(|c| Box::new(c) as Box<dyn CodecTrait>)
                    .map_err(|e| WebRtcGlueError::Codec(e.to_string()))
            }
            #[cfg(not(feature = "opus"))]
            {
                let _ = rate;
                Err(WebRtcGlueError::Codec(
                    "Opus was negotiated but this build has no Opus codec \
                     (enable the `opus` feature)"
                        .into(),
                ))
            }
        }
        other => Err(WebRtcGlueError::Codec(format!(
            "{other:?} is not a WebRTC leg codec"
        ))),
    }
}

/// Browser → bridge. Feed it RTP; take PCM16LE frames of exactly the
/// bridge's 20 ms size.
pub struct InboundAudio {
    codec: Box<dyn CodecTrait>,
    jitter: JitterBuffer,
    reframer: Reframer,
    payload_type: u8,
    /// Packets whose payload type is not the negotiated one (a browser
    /// sending DTMF or comfort noise on the same stream). Counted, not
    /// decoded — feeding telephone-event bytes to a speech codec would
    /// produce noise.
    pub other_payload_packets: u64,
    /// Payloads the codec refused. A decode failure is per-packet and
    /// recoverable; killing the call over one is worse than a click.
    pub decode_errors: u64,
}

impl InboundAudio {
    pub fn new(codec: AudioCodec, payload_type: u8, bridge_rate: u32) -> Result<Self> {
        Ok(Self {
            codec: codec_for(codec, bridge_rate)?,
            jitter: JitterBuffer::new(JITTER_TARGET),
            reframer: Reframer::new(bridge_rate)
                .map_err(|e| WebRtcGlueError::Codec(e.to_string()))?,
            payload_type,
            other_payload_packets: 0,
            decode_errors: 0,
        })
    }

    /// Accept one inbound packet. Reordering and duplicate suppression
    /// are the jitter buffer's job; nothing is decoded yet.
    pub fn push(&mut self, packet: &RtpPacket) {
        if packet.header.payload_type() != self.payload_type {
            self.other_payload_packets += 1;
            return;
        }
        self.jitter.push(
            packet.header.sequence_number,
            packet.header.timestamp,
            packet.payload.to_vec(),
        );
    }

    /// Drain whatever the jitter buffer will release, decode it, and
    /// return the complete 20 ms PCM16LE frames that fall out.
    ///
    /// **Call this on a 20 ms tick, not on packet arrival.** The
    /// buffer releases a packet only once it has been held for
    /// [`JITTER_TARGET`], which is exactly how arrival jitter is
    /// converted into the steady cadence the bridge expects — the
    /// same shape as the classic tap's pacing tick. Draining only when
    /// a packet arrives would hand the jitter straight through and,
    /// on a quiet stream, stall until the next packet.
    ///
    /// Returns frames in the shape `caller_audio_tx` wants, so the
    /// caller's send loop is identical to the classic tap's.
    pub fn drain(&mut self) -> Vec<Vec<u8>> {
        let mut frames = Vec::new();
        while let Some(payload) = self.jitter.pop() {
            match self.codec.decode(&payload) {
                Ok(samples) => self.reframer.push(&samples),
                Err(e) => {
                    self.decode_errors += 1;
                    // One bad packet is a click, not a dropped call.
                    debug!(error = %e, "WebRTC leg: dropping undecodable payload");
                    continue;
                }
            }
            while let Some(frame) = self.reframer.pop_frame() {
                frames.push(pack_pcm16_le(&frame));
            }
        }
        frames
    }

    /// Jitter-buffer depth, for the setup watchdog and metrics.
    pub fn buffered_packets(&self) -> usize {
        self.jitter.size()
    }
}

/// Bridge → browser. Feed it the WS server's PCM16LE frames; it
/// encodes and sends them as SRTP.
pub struct OutboundAudio {
    codec: Box<dyn CodecTrait>,
    sender: AudioSender,
    /// Timestamp units per 20 ms **at the codec's RTP clock** — 960 for
    /// Opus (48 kHz clock even when decoding at 16 kHz, RFC 7587 §4.1),
    /// 160 for G.711. Taken from forge-webrtc, never derived from the
    /// PCM frame length; see the module docs.
    samples_per_frame: u32,
    /// Frames the codec refused to encode.
    pub encode_errors: u64,
}

impl OutboundAudio {
    pub fn new(codec: AudioCodec, sender: AudioSender, bridge_rate: u32) -> Result<Self> {
        let samples_per_frame = sender.samples_per_20ms();
        Ok(Self {
            codec: codec_for(codec, bridge_rate)?,
            sender,
            samples_per_frame,
            encode_errors: 0,
        })
    }

    /// Encode and transmit one 20 ms PCM16LE frame from the bridge.
    ///
    /// The frame is exactly the size the WS contract already enforces
    /// (`conn.rs` rejects anything else before it reaches a leg), so a
    /// wrong size here is a bug rather than a peer's fault — hence the
    /// hard error rather than a silent drop.
    pub async fn send_frame(&mut self, pcm16le: &[u8]) -> Result<()> {
        let samples =
            unpack_pcm16_le(pcm16le).map_err(|e| WebRtcGlueError::Codec(e.to_string()))?;
        let encoded = match self.codec.encode(&samples) {
            Ok(e) => e,
            Err(e) => {
                self.encode_errors += 1;
                warn!(error = %e, "WebRTC leg: dropping unencodable frame");
                return Ok(());
            }
        };
        self.sender
            .send_audio(Bytes::from(encoded), self.samples_per_frame)
            .await
            .map_err(|e| WebRtcGlueError::Send(e.to_string()))
    }

    /// Timestamp units this leg advances per 20 ms frame.
    pub fn samples_per_frame(&self) -> u32 {
        self.samples_per_frame
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use forge_rtp::{RtpHeader, RtpPacket};

    fn g711_packet(seq: u16, ts: u32, pt: u8, payload: Vec<u8>) -> RtpPacket {
        let header = RtpHeader {
            version_flags: 0x80,     // V=2, no padding/extension/CSRC
            marker_payload_type: pt, // marker clear
            sequence_number: seq,
            timestamp: ts,
            ssrc: 0x1234_5678,
        };
        RtpPacket {
            header,
            csrc_list: vec![],
            extension: None,
            payload: Bytes::from(payload),
            padding_len: 0,
        }
    }

    /// The buffer is wall-clock gated, so a test must wait out
    /// [`JITTER_TARGET`] before draining — exactly as the real leg's
    /// 20 ms tick does.
    async fn settle() {
        tokio::time::sleep(JITTER_TARGET + Duration::from_millis(20)).await;
    }

    /// 20 ms of µ-law is 160 bytes → exactly one 8 kHz bridge frame of
    /// 160 samples = 320 bytes PCM16LE.
    #[tokio::test]
    async fn g711_packets_become_exact_bridge_frames() {
        let mut inbound = InboundAudio::new(AudioCodec::PCMU, 0, 8_000).expect("codec");
        for i in 0..4u16 {
            inbound.push(&g711_packet(i, u32::from(i) * 160, 0, vec![0xffu8; 160]));
        }
        settle().await;
        let frames = inbound.drain();
        assert!(!frames.is_empty(), "some frames must be released");
        for f in &frames {
            assert_eq!(f.len(), 320, "20 ms @ 8 kHz PCM16LE");
        }
        assert_eq!(inbound.decode_errors, 0);
    }

    /// A browser sends telephone-event on the same stream. Those bytes
    /// are not speech and must never reach the decoder.
    #[test]
    fn foreign_payload_types_are_counted_not_decoded() {
        let mut inbound = InboundAudio::new(AudioCodec::PCMU, 0, 8_000).expect("codec");
        inbound.push(&g711_packet(1, 160, 101, vec![0x00, 0x0a, 0x00, 0xa0]));
        assert_eq!(inbound.other_payload_packets, 1);
        assert_eq!(inbound.buffered_packets(), 0, "nothing was buffered");
        assert!(inbound.drain().is_empty());
    }

    /// Out-of-order arrival is the jitter buffer's job: 1, 3, 2 on the
    /// wire must reach the bridge as 1, 2, 3.
    ///
    /// Note what "late" means here — the buffer takes its baseline from
    /// the first packet it sees, so a packet *preceding* that baseline
    /// is late by definition and is dropped rather than resequenced
    /// (the case below). Real reordering happens after the stream is
    /// established, which is what this covers.
    #[tokio::test]
    async fn reordered_packets_are_resequenced() {
        let mut inbound = InboundAudio::new(AudioCodec::PCMU, 0, 8_000).expect("codec");
        // Constant payloads: after µ-law decode each frame is a
        // constant sample value, so release order is observable.
        inbound.push(&g711_packet(1, 160, 0, vec![0x01u8; 160]));
        inbound.push(&g711_packet(3, 480, 0, vec![0x03u8; 160])); // early
        inbound.push(&g711_packet(2, 320, 0, vec![0x02u8; 160])); // fills the hole
        settle().await;

        let frames = inbound.drain();
        assert_eq!(frames.len(), 3, "all three released");
        let order: Vec<i16> = frames.iter().map(|f| decode_first_sample(f)).collect();
        assert_eq!(
            order,
            vec![
                forge_codecs::g711::decode_ulaw(0x01),
                forge_codecs::g711::decode_ulaw(0x02),
                forge_codecs::g711::decode_ulaw(0x03),
            ],
            "sequence order, not arrival order"
        );
        assert_eq!(inbound.decode_errors, 0);
    }

    /// A packet arriving *behind* the stream's baseline is late, not
    /// reorderable: its playout moment never existed, so it is dropped
    /// rather than replayed out of position. Before forge-media #132
    /// this exact shape aborted the process with a stack overflow.
    #[tokio::test]
    async fn a_packet_behind_the_baseline_is_dropped_not_replayed() {
        let mut inbound = InboundAudio::new(AudioCodec::PCMU, 0, 8_000).expect("codec");
        inbound.push(&g711_packet(2, 320, 0, vec![0x02u8; 160]));
        inbound.push(&g711_packet(1, 160, 0, vec![0x01u8; 160]));
        settle().await;
        let frames = inbound.drain();
        assert_eq!(frames.len(), 1, "only the baseline packet plays");
        assert_eq!(
            decode_first_sample(&frames[0]),
            forge_codecs::g711::decode_ulaw(0x02)
        );
    }

    /// First PCM16LE sample of a frame, as i16.
    fn decode_first_sample(frame: &[u8]) -> i16 {
        i16::from_le_bytes([frame[0], frame[1]])
    }

    /// A gap the sender never fills must not wedge the stream: the
    /// buffer steps over the hole once the wait exceeds its max delay
    /// (3× the target), so audio keeps flowing.
    #[tokio::test]
    async fn a_permanent_gap_does_not_wedge_the_stream() {
        let mut inbound = InboundAudio::new(AudioCodec::PCMU, 0, 8_000).expect("codec");
        inbound.push(&g711_packet(1, 160, 0, vec![0x01u8; 160]));
        settle().await;
        assert_eq!(inbound.drain().len(), 1);

        // seq 2 is lost forever; 3 and 4 arrive behind it.
        inbound.push(&g711_packet(3, 480, 0, vec![0x03u8; 160]));
        inbound.push(&g711_packet(4, 640, 0, vec![0x04u8; 160]));
        tokio::time::sleep(JITTER_TARGET * 3 + Duration::from_millis(40)).await;

        let frames = inbound.drain();
        assert_eq!(frames.len(), 2, "the stream continues past the hole");
        assert_eq!(
            decode_first_sample(&frames[0]),
            forge_codecs::g711::decode_ulaw(0x03),
            "resumes at the first packet after the gap"
        );
    }

    #[test]
    fn unsupported_bridge_rate_is_refused_before_audio_flows() {
        // 44.1 kHz is not in the bridge's fixed 8/16 kHz contract.
        let err = match InboundAudio::new(AudioCodec::PCMU, 0, 44_100) {
            Err(e) => e,
            Ok(_) => panic!("44.1 kHz is outside the bridge's 8/16 kHz contract"),
        };
        assert!(matches!(err, WebRtcGlueError::Codec(_)), "{err:?}");
    }

    #[test]
    fn g722_is_not_a_webrtc_leg_codec() {
        let err = match InboundAudio::new(AudioCodec::G722, 9, 16_000) {
            Err(e) => e,
            Ok(_) => panic!("G.722 is not a WebRTC leg codec"),
        };
        assert!(matches!(err, WebRtcGlueError::Codec(_)), "{err:?}");
    }
}
