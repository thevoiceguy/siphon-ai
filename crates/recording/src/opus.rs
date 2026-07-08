//! Ogg-Opus recording output (`[recording].format = "opus"`, 0.25.0 —
//! DESIGN_RECORDING_COMPLIANCE §5 D4).
//!
//! Encodes each 20 ms stereo frame with libopus (the same native library
//! forge uses for call audio) and encapsulates the packets in an Ogg
//! stream per RFC 7845. ~10× smaller than WAV for voice, and — unlike
//! WAV — **streaming-native**: nothing in the container needs a finalize
//! back-patch, so the encrypted-envelope path needs no chunk-0 rewrite
//! for Opus recordings.
//!
//! The stream layout is the standard three-part shape: an `OpusHead`
//! identification packet (its own page), an `OpusTags` comment packet
//! (its own page), then audio packets with granule positions counted in
//! 48 kHz samples (960 per 20 ms frame) offset by the encoder pre-skip.

use audiopus::coder::Encoder;
use audiopus::{Application, Channels, SampleRate};
use ogg::writing::{PacketWriteEndInfo, PacketWriter};
use thiserror::Error;

/// 20 ms at 48 kHz — the granule increment per frame, regardless of the
/// encoder's input rate (RFC 7845 §4).
const GRANULE_PER_FRAME: u64 = 960;
/// Max size of one encoded 20 ms packet. Voice packets are ~100–400
/// bytes; 4000 is libopus' own recommended ceiling.
const MAX_PACKET: usize = 4000;
/// Arbitrary but fixed Ogg bitstream serial.
const SERIAL: u32 = 0x5150_4153; // "SAPQ"

#[derive(Debug, Error)]
pub enum OpusError {
    #[error("opus encoder: {0}")]
    Codec(#[from] audiopus::Error),
    #[error("ogg write: {0}")]
    Ogg(#[from] std::io::Error),
    #[error("unsupported sample rate {0} for opus recording (8000 or 16000)")]
    UnsupportedRate(u32),
}

/// A streaming Ogg-Opus encoder. Feed one 20 ms stereo frame per
/// [`Self::encode_frame`]; drain bytes with [`Self::take_bytes`]; call
/// [`Self::finish`] to close the stream.
pub struct OggOpusStream {
    encoder: Encoder,
    writer: PacketWriter<'static, Vec<u8>>,
    /// Interleaved-stereo i16 scratch for one frame (reused, no
    /// steady-state allocation).
    frame: Vec<i16>,
    packet: Vec<u8>,
    samples_per_channel: usize,
    granule: u64,
    finished: bool,
}

impl OggOpusStream {
    pub fn new(sample_rate: u32) -> Result<Self, OpusError> {
        let (rate, samples_per_channel) = match sample_rate {
            8000 => (SampleRate::Hz8000, 160usize),
            16000 => (SampleRate::Hz16000, 320usize),
            other => return Err(OpusError::UnsupportedRate(other)),
        };
        let encoder = Encoder::new(rate, Channels::Stereo, Application::Voip)?;
        // Pre-skip is the decoder-discard lead-in, expressed in 48 kHz
        // samples (RFC 7845 §5.1): the encoder lookahead scaled up from
        // the input rate.
        let lookahead = encoder.lookahead()? as u64;
        let pre_skip = (lookahead * 48_000 / sample_rate as u64) as u16;

        let mut writer = PacketWriter::new(Vec::with_capacity(64 * 1024));
        writer.write_packet(
            opus_head(sample_rate, pre_skip),
            SERIAL,
            PacketWriteEndInfo::EndPage,
            0,
        )?;
        writer.write_packet(opus_tags(), SERIAL, PacketWriteEndInfo::EndPage, 0)?;

        Ok(Self {
            encoder,
            writer,
            frame: vec![0i16; samples_per_channel * 2],
            packet: vec![0u8; MAX_PACKET],
            samples_per_channel,
            granule: u64::from(pre_skip),
            finished: false,
        })
    }

    /// Encode one 20 ms tick: `caller` left, `bot` right, each
    /// `mono_bytes` of PCM16-LE (short/missing legs are silence-padded,
    /// mirroring the WAV writer's contract).
    pub fn encode_frame(
        &mut self,
        caller: Option<&[u8]>,
        bot: Option<&[u8]>,
    ) -> Result<(), OpusError> {
        let sample = |src: Option<&[u8]>, i: usize| -> i16 {
            match src {
                Some(s) if i * 2 + 1 < s.len() => i16::from_le_bytes([s[i * 2], s[i * 2 + 1]]),
                _ => 0,
            }
        };
        for i in 0..self.samples_per_channel {
            self.frame[i * 2] = sample(caller, i);
            self.frame[i * 2 + 1] = sample(bot, i);
        }
        let n = self.encoder.encode(&self.frame, &mut self.packet)?;
        self.granule += GRANULE_PER_FRAME;
        self.writer.write_packet(
            self.packet[..n].to_vec(),
            SERIAL,
            PacketWriteEndInfo::NormalPacket,
            self.granule,
        )?;
        Ok(())
    }

    /// Bytes finished so far (drains the internal buffer).
    pub fn take_bytes(&mut self) -> Vec<u8> {
        std::mem::take(self.writer.inner_mut())
    }

    /// Close the stream (writes the end-of-stream page) and return the
    /// remaining bytes.
    pub fn finish(mut self) -> Result<Vec<u8>, OpusError> {
        // An empty packet is not valid Opus; close the stream on a final
        // silent frame so the EOS flag rides a real packet.
        let n = {
            self.frame.fill(0);
            self.encoder.encode(&self.frame, &mut self.packet)?
        };
        self.granule += GRANULE_PER_FRAME;
        self.writer.write_packet(
            self.packet[..n].to_vec(),
            SERIAL,
            PacketWriteEndInfo::EndStream,
            self.granule,
        )?;
        self.finished = true;
        Ok(std::mem::take(self.writer.inner_mut()))
    }
}

/// RFC 7845 §5.1 identification header.
fn opus_head(input_rate: u32, pre_skip: u16) -> Vec<u8> {
    let mut h = Vec::with_capacity(19);
    h.extend_from_slice(b"OpusHead");
    h.push(1); // version
    h.push(2); // channels (stereo: caller L, bot R)
    h.extend_from_slice(&pre_skip.to_le_bytes());
    h.extend_from_slice(&input_rate.to_le_bytes());
    h.extend_from_slice(&0i16.to_le_bytes()); // output gain
    h.push(0); // channel mapping family 0
    h
}

/// RFC 7845 §5.2 comment header.
fn opus_tags() -> Vec<u8> {
    let vendor = b"siphon-ai";
    let mut t = Vec::with_capacity(8 + 4 + vendor.len() + 4);
    t.extend_from_slice(b"OpusTags");
    t.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
    t.extend_from_slice(vendor);
    t.extend_from_slice(&0u32.to_le_bytes()); // zero user comments
    t
}

#[cfg(test)]
mod tests {
    use super::*;
    use audiopus::coder::Decoder;

    /// Encode a short recording, then parse the Ogg stream back and
    /// decode every audio packet — structural + codec-level validation
    /// without any external tool.
    #[test]
    fn stream_parses_and_decodes() {
        let mut s = OggOpusStream::new(16000).unwrap();
        // 25 frames (~0.5 s) of a loud square-ish signal on the left leg.
        let caller: Vec<u8> = (0..640)
            .map(|i| if i % 4 < 2 { 0x40 } else { 0xC0 })
            .collect();
        let mut bytes = Vec::new();
        for _ in 0..25 {
            s.encode_frame(Some(&caller), None).unwrap();
            bytes.extend(s.take_bytes());
        }
        bytes.extend(s.finish().unwrap());

        assert_eq!(&bytes[..4], b"OggS", "must be an Ogg stream");

        let mut reader = ogg::PacketReader::new(std::io::Cursor::new(&bytes));
        let head = reader.read_packet_expected().unwrap();
        assert_eq!(&head.data[..8], b"OpusHead");
        assert_eq!(head.data[9], 2, "stereo");
        let tags = reader.read_packet_expected().unwrap();
        assert_eq!(&tags.data[..8], b"OpusTags");

        let mut decoder = Decoder::new(SampleRate::Hz16000, Channels::Stereo).unwrap();
        let mut pcm = vec![0i16; 320 * 2];
        let mut audio_packets = 0;
        while let Some(packet) = reader.read_packet().unwrap() {
            let samples = decoder
                .decode(
                    Some(audiopus::packet::Packet::try_from(&packet.data).unwrap()),
                    audiopus::MutSignals::try_from(&mut pcm).unwrap(),
                    false,
                )
                .expect("every audio packet must decode");
            assert_eq!(samples, 320, "20 ms per packet at 16 kHz");
            audio_packets += 1;
        }
        assert_eq!(audio_packets, 26, "25 frames + the EOS flush frame");
    }

    #[test]
    fn unsupported_rate_fails_loud() {
        assert!(matches!(
            OggOpusStream::new(44_100),
            Err(OpusError::UnsupportedRate(44_100))
        ));
    }
}
