//! The per-call recording writer task.
//!
//! Mixes the two tapped legs into a stereo WAV on a 20 ms monotonic clock.
//! It runs as its own per-call task — **never on the audio hot path**
//! (CLAUDE.md §4.3): the tap `try_send`s frame copies to the bounded
//! channel this task drains, and the (batched) file I/O happens here.
//!
//! Layout: dual-channel stereo PCM16-LE — caller **left**, bot **right**.
//! Each 20 ms tick (while recording) emits one stereo frame from the most
//! recent frame seen for each leg, or silence for a leg that produced
//! nothing — so the recording tracks the call's wall clock.
//!
//! Control: `auto_start = true` (mode `always`) records the whole call.
//! Otherwise the writer starts **idle** and a [`RecControl::Start`] begins
//! it. [`RecControl::Pause`] **omits** the paused span (it stops writing —
//! the paused audio is dropped, not silenced — for PCI "pause while the
//! caller reads a card number"); `Resume` continues; `Stop` finalizes early.

use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use std::time::Duration;

use thiserror::Error;
use tokio::fs::File;
use tokio::io::{AsyncSeekExt, AsyncWriteExt, BufWriter};
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::control::{RecControl, RecEvent};
use crate::envelope::EnvelopeWriter;
use crate::frame::RecFrame;
use crate::kek::Kek;

/// Recording cadence — one stereo frame per 20 ms, matching the bridge.
const FRAME_MS: u64 = 20;
/// Flush the in-memory buffer to disk once it reaches this size (~1 s of
/// stereo 16 kHz audio). Batches syscalls so the writer task rarely blocks.
const FLUSH_BYTES: usize = 64 * 1024;
/// WAV header length (canonical 44-byte PCM header).
const WAV_HEADER_LEN: usize = 44;

/// Outcome of a finished recording.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordingStats {
    pub path: PathBuf,
    pub frames: u64,
    pub data_bytes: u64,
}

#[derive(Debug, Error)]
pub enum RecordingError {
    #[error("recording I/O failed for {path:?}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("unsupported sample rate {0} (8000 or 16000 only)")]
    UnsupportedSampleRate(u32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Idle,
    Recording,
    Paused,
    Done,
}

/// Per-call recording writer. Build with [`RecordingWriter::new`], then
/// drive with [`RecordingWriter::run`].
pub struct RecordingWriter {
    path: PathBuf,
    sample_rate: u32,
    auto_start: bool,
    encryption: Option<Kek>,
}

impl RecordingWriter {
    /// `auto_start = true` begins recording immediately (mode `always`);
    /// `false` waits for a [`RecControl::Start`] (mode `on_demand`).
    pub fn new(path: PathBuf, sample_rate: u32, auto_start: bool) -> Self {
        Self {
            path,
            sample_rate,
            auto_start,
            encryption: None,
        }
    }

    /// Encrypt the recording at rest: seal the WAV payload into a `.wava`
    /// envelope under a per-recording DEK wrapped by `kek` (0.24.0).
    /// `None` (the default) keeps plaintext WAV output.
    pub fn with_encryption(mut self, kek: Option<Kek>) -> Self {
        self.encryption = kek;
        self
    }

    /// Run until `audio_rx` closes (call ended). `ctrl_rx` drives the
    /// recording state machine; lifecycle events are reported on `evt_tx`.
    /// Returns the stats if anything was recorded, or `None` (on-demand
    /// never started). An I/O failure returns `Err` and emits `Failed`.
    pub async fn run(
        self,
        mut audio_rx: mpsc::Receiver<RecFrame>,
        mut ctrl_rx: mpsc::Receiver<RecControl>,
        evt_tx: mpsc::Sender<RecEvent>,
    ) -> Result<Option<RecordingStats>, RecordingError> {
        let mono_bytes = match self.sample_rate {
            8000 => 320usize,  // 160 samples * 2 bytes
            16000 => 640usize, // 320 samples * 2 bytes
            other => return Err(RecordingError::UnsupportedSampleRate(other)),
        };

        let mut open: Option<Open> = None;
        let mut status = Status::Idle;
        let mut latest_caller: Option<Vec<u8>> = None;
        let mut latest_bot: Option<Vec<u8>> = None;
        let mut ctrl_open = true;
        let mut last_stats: Option<RecordingStats> = None;

        // Helper to surface an I/O failure: emit `Failed`, then return Err.
        macro_rules! fail {
            ($e:expr) => {{
                let err = RecordingError::Io {
                    path: self.path.clone(),
                    source: $e,
                };
                let _ = evt_tx
                    .send(RecEvent::Failed {
                        reason: err.to_string(),
                    })
                    .await;
                return Err(err);
            }};
        }

        if self.auto_start {
            match Open::create(&self.path, self.sample_rate, self.encryption.as_ref()).await {
                Ok(o) => {
                    open = Some(o);
                    status = Status::Recording;
                    let _ = evt_tx.send(RecEvent::Started).await;
                }
                Err(e) => fail!(e),
            }
        }

        let mut tick = tokio::time::interval(Duration::from_millis(FRAME_MS));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                biased;
                maybe = audio_rx.recv() => match maybe {
                    Some(RecFrame::Caller(b)) => latest_caller = Some(b),
                    Some(RecFrame::Bot(b)) => latest_bot = Some(b),
                    None => break, // call over
                },
                maybe = ctrl_rx.recv(), if ctrl_open => match maybe {
                    Some(RecControl::Start) if status == Status::Idle => {
                        match Open::create(&self.path, self.sample_rate, self.encryption.as_ref()).await {
                            Ok(o) => {
                                open = Some(o);
                                status = Status::Recording;
                                let _ = evt_tx.send(RecEvent::Started).await;
                            }
                            Err(e) => fail!(e),
                        }
                    }
                    Some(RecControl::Pause) if status == Status::Recording => {
                        status = Status::Paused;
                    }
                    Some(RecControl::Resume) if status == Status::Paused => {
                        status = Status::Recording;
                    }
                    Some(RecControl::Stop)
                        if matches!(status, Status::Recording | Status::Paused) =>
                    {
                        if let Some(o) = open.take() {
                            match o.finalize().await {
                                Ok(stats) => {
                                    let _ = evt_tx.send(RecEvent::Stopped {
                                        data_bytes: stats.data_bytes,
                                        frames: stats.frames,
                                    }).await;
                                    last_stats = Some(stats);
                                }
                                Err(e) => fail!(e),
                            }
                        }
                        status = Status::Done;
                    }
                    Some(_) => {} // control invalid for the current state — ignore
                    None => ctrl_open = false,
                },
                _ = tick.tick() => {
                    let caller = latest_caller.take();
                    let bot = latest_bot.take();
                    if status == Status::Recording {
                        if let Some(o) = open.as_mut() {
                            if let Err(e) = o.write_frame(caller.as_deref(), bot.as_deref(), mono_bytes).await {
                                fail!(e);
                            }
                        }
                    }
                    // Paused/Idle/Done: drop the frames (pause omits the span).
                }
            }
        }

        // Call ended while still recording/paused → finalize it.
        if let Some(o) = open.take() {
            match o.finalize().await {
                Ok(stats) => {
                    let _ = evt_tx
                        .send(RecEvent::Stopped {
                            data_bytes: stats.data_bytes,
                            frames: stats.frames,
                        })
                        .await;
                    last_stats = Some(stats);
                }
                Err(e) => fail!(e),
            }
        }
        Ok(last_stats)
    }
}

/// The in-progress path: `<final>.part`, renamed onto the final path only
/// by a successful finalize — so a bare `.wav`/`.wava` on disk is always a
/// complete recording, and a crash leaves only a `.part` (0.24.0, §6 D5).
fn part_path(path: &Path) -> PathBuf {
    let mut os = path.as_os_str().to_os_string();
    os.push(".part");
    PathBuf::from(os)
}

/// Where the WAV bytes go: straight to disk, or sealed into an encrypted
/// envelope first. Plaintext keeps the seek-back header patch; the envelope
/// patches via its chunk-0 rewrite instead (no seeking into ciphertext).
enum Output {
    Plain(BufWriter<File>),
    Sealed(Box<EnvelopeWriter>),
}

/// An open recording mid-write (WAV, possibly enveloped).
struct Open {
    path: PathBuf,
    part: PathBuf,
    sample_rate: u32,
    out: Output,
    buf: Vec<u8>,
    frames: u64,
    data_bytes: u64,
}

impl Open {
    async fn create(
        path: &Path,
        sample_rate: u32,
        encryption: Option<&Kek>,
    ) -> std::io::Result<Self> {
        let part = part_path(path);
        let file = File::create(&part).await?;
        let out = match encryption {
            None => {
                let mut out = BufWriter::new(file);
                out.write_all(&wav_header(sample_rate, 0)).await?; // placeholder
                Output::Plain(out)
            }
            Some(kek) => {
                let mut env = EnvelopeWriter::create(&part, file, kek)
                    .await
                    .map_err(std::io::Error::other)?;
                env.write(&wav_header(sample_rate, 0)) // placeholder, patched at finalize
                    .await
                    .map_err(std::io::Error::other)?;
                Output::Sealed(Box::new(env))
            }
        };
        Ok(Self {
            path: path.to_path_buf(),
            part,
            sample_rate,
            out,
            buf: Vec::with_capacity(FLUSH_BYTES * 2),
            frames: 0,
            data_bytes: 0,
        })
    }

    async fn flush_buf(&mut self) -> std::io::Result<()> {
        let buf = std::mem::take(&mut self.buf);
        match &mut self.out {
            Output::Plain(out) => out.write_all(&buf).await?,
            Output::Sealed(env) => env.write(&buf).await.map_err(std::io::Error::other)?,
        }
        self.buf = buf;
        self.buf.clear();
        Ok(())
    }

    async fn write_frame(
        &mut self,
        caller: Option<&[u8]>,
        bot: Option<&[u8]>,
        mono_bytes: usize,
    ) -> std::io::Result<()> {
        interleave_into(&mut self.buf, caller, bot, mono_bytes);
        self.frames += 1;
        self.data_bytes += (mono_bytes * 2) as u64;
        if self.buf.len() >= FLUSH_BYTES {
            self.flush_buf().await?;
        }
        Ok(())
    }

    async fn finalize(mut self) -> std::io::Result<RecordingStats> {
        if !self.buf.is_empty() {
            self.flush_buf().await?;
        }
        let data_u32 = u32::try_from(self.data_bytes).unwrap_or(u32::MAX);
        let riff_u32 = 36u32.wrapping_add(data_u32);
        match self.out {
            Output::Plain(mut out) => {
                out.flush().await?;
                out.seek(SeekFrom::Start(4)).await?;
                out.write_all(&riff_u32.to_le_bytes()).await?;
                out.seek(SeekFrom::Start(40)).await?;
                out.write_all(&data_u32.to_le_bytes()).await?;
                out.flush().await?;
            }
            Output::Sealed(env) => {
                env.finalize(&[(4, riff_u32), (40, data_u32)])
                    .await
                    .map_err(std::io::Error::other)?;
            }
        }
        tokio::fs::rename(&self.part, &self.path).await?;
        debug!(path = %self.path.display(), frames = self.frames, data_bytes = self.data_bytes, "recording finalized");
        if self.data_bytes > u32::MAX as u64 {
            warn!(path = %self.path.display(), "recording exceeded 4 GiB; WAV header sizes saturated");
        }
        let _ = self.sample_rate;
        Ok(RecordingStats {
            path: self.path,
            frames: self.frames,
            data_bytes: self.data_bytes,
        })
    }
}

/// Append one interleaved stereo frame (L = caller, R = bot) to `buf`.
/// Each leg is taken as exactly `mono_bytes` (truncated or zero-padded);
/// a `None` leg is silence.
fn interleave_into(
    buf: &mut Vec<u8>,
    caller: Option<&[u8]>,
    bot: Option<&[u8]>,
    mono_bytes: usize,
) {
    let sample = |src: Option<&[u8]>, i: usize| -> [u8; 2] {
        match src {
            Some(s) if i + 1 < s.len() => [s[i], s[i + 1]],
            _ => [0, 0],
        }
    };
    let mut i = 0;
    while i < mono_bytes {
        buf.extend_from_slice(&sample(caller, i)); // left
        buf.extend_from_slice(&sample(bot, i)); // right
        i += 2;
    }
}

/// Canonical 44-byte PCM WAV header for stereo 16-bit at `sample_rate`,
/// with `data_len` bytes of sample data (0 for the streaming placeholder).
fn wav_header(sample_rate: u32, data_len: u32) -> [u8; WAV_HEADER_LEN] {
    const CHANNELS: u16 = 2;
    const BITS: u16 = 16;
    let block_align: u16 = CHANNELS * (BITS / 8);
    let byte_rate: u32 = sample_rate * block_align as u32;
    let mut h = [0u8; WAV_HEADER_LEN];
    h[0..4].copy_from_slice(b"RIFF");
    h[4..8].copy_from_slice(&36u32.wrapping_add(data_len).to_le_bytes());
    h[8..12].copy_from_slice(b"WAVE");
    h[12..16].copy_from_slice(b"fmt ");
    h[16..20].copy_from_slice(&16u32.to_le_bytes()); // fmt chunk size
    h[20..22].copy_from_slice(&1u16.to_le_bytes()); // PCM
    h[22..24].copy_from_slice(&CHANNELS.to_le_bytes());
    h[24..28].copy_from_slice(&sample_rate.to_le_bytes());
    h[28..32].copy_from_slice(&byte_rate.to_le_bytes());
    h[32..34].copy_from_slice(&block_align.to_le_bytes());
    h[34..36].copy_from_slice(&BITS.to_le_bytes());
    h[36..40].copy_from_slice(b"data");
    h[40..44].copy_from_slice(&data_len.to_le_bytes());
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("siphon_rec_{}_{}", std::process::id(), tag));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("c.wav")
    }

    fn read_data_len(path: &PathBuf) -> usize {
        let b = std::fs::read(path).unwrap();
        assert_eq!(&b[0..4], b"RIFF");
        assert_eq!(&b[8..12], b"WAVE");
        assert_eq!(u16::from_le_bytes(b[22..24].try_into().unwrap()), 2);
        let data = u32::from_le_bytes(b[40..44].try_into().unwrap()) as usize;
        assert_eq!(b.len(), WAV_HEADER_LEN + data); // header sizes consistent
        data
    }

    #[test]
    fn header_is_well_formed() {
        let h = wav_header(8000, 320);
        assert_eq!(&h[0..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(h[4..8].try_into().unwrap()), 36 + 320);
        assert_eq!(u32::from_le_bytes(h[40..44].try_into().unwrap()), 320);
        assert_eq!(u32::from_le_bytes(h[28..32].try_into().unwrap()), 32000); // byte rate
    }

    #[test]
    fn interleave_pairs_legs_left_right() {
        let mut buf = Vec::new();
        interleave_into(
            &mut buf,
            Some(&[0x11, 0x11, 0x22, 0x22]),
            Some(&[0x33, 0x33, 0x44, 0x44]),
            4,
        );
        assert_eq!(buf, vec![0x11, 0x11, 0x33, 0x33, 0x22, 0x22, 0x44, 0x44]);
    }

    #[tokio::test]
    async fn always_records_whole_call() {
        let path = temp_path("always");
        let (atx, arx) = mpsc::channel(64);
        let (_ctx, crx) = mpsc::channel(8);
        let (etx, mut erx) = mpsc::channel(8);
        let h = tokio::spawn(RecordingWriter::new(path.clone(), 8000, true).run(arx, crx, etx));
        // First event is Started (auto-start).
        assert!(matches!(erx.recv().await, Some(RecEvent::Started)));
        for _ in 0..5 {
            atx.send(RecFrame::Caller(vec![1u8; 320])).await.unwrap();
            atx.send(RecFrame::Bot(vec![2u8; 320])).await.unwrap();
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        drop(atx);
        let stats = h.await.unwrap().unwrap().unwrap();
        assert!(stats.frames >= 4);
        assert!(matches!(erx.recv().await, Some(RecEvent::Stopped { .. })));
        assert_eq!(read_data_len(&path) as u64, stats.data_bytes);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[tokio::test]
    async fn on_demand_idle_until_start_then_stop() {
        let path = temp_path("ondemand");
        let (atx, arx) = mpsc::channel(64);
        let (ctx, crx) = mpsc::channel(8);
        let (etx, mut erx) = mpsc::channel(8);
        let h = tokio::spawn(RecordingWriter::new(path.clone(), 8000, false).run(arx, crx, etx));
        // No file yet — feed audio while idle; it's dropped.
        atx.send(RecFrame::Caller(vec![1u8; 320])).await.unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(!path.exists(), "no file before Start");
        ctx.send(RecControl::Start).await.unwrap();
        assert!(matches!(erx.recv().await, Some(RecEvent::Started)));
        for _ in 0..4 {
            atx.send(RecFrame::Caller(vec![1u8; 320])).await.unwrap();
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        ctx.send(RecControl::Stop).await.unwrap();
        assert!(matches!(erx.recv().await, Some(RecEvent::Stopped { .. })));
        // Audio after Stop is ignored.
        atx.send(RecFrame::Caller(vec![1u8; 320])).await.unwrap();
        drop(atx);
        let stats = h.await.unwrap().unwrap().unwrap();
        assert!(stats.frames >= 2);
        assert_eq!(read_data_len(&path) as u64, stats.data_bytes);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[tokio::test]
    async fn part_file_until_finalize_then_bare_path() {
        let path = temp_path("part");
        let (atx, arx) = mpsc::channel(64);
        let (_ctx, crx) = mpsc::channel(8);
        let (etx, mut erx) = mpsc::channel(8);
        let h = tokio::spawn(RecordingWriter::new(path.clone(), 8000, true).run(arx, crx, etx));
        assert!(matches!(erx.recv().await, Some(RecEvent::Started)));
        // Mid-recording: only the .part exists (0.24.0 finalize atomicity).
        atx.send(RecFrame::Caller(vec![1u8; 320])).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(part_path(&path).exists(), ".part must exist mid-recording");
        assert!(!path.exists(), "final path must not exist mid-recording");
        drop(atx);
        h.await.unwrap().unwrap().unwrap();
        assert!(path.exists(), "final path must exist after finalize");
        assert!(!part_path(&path).exists(), ".part must be renamed away");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[tokio::test]
    async fn encrypted_recording_decrypts_to_valid_wav() {
        let path = temp_path("encrypted").with_extension("wava");
        let kek = crate::kek::Kek::new_static([5u8; 32], "unit-kek".into());
        let (atx, arx) = mpsc::channel(64);
        let (_ctx, crx) = mpsc::channel(8);
        let (etx, mut erx) = mpsc::channel(8);
        let h = tokio::spawn(
            RecordingWriter::new(path.clone(), 8000, true)
                .with_encryption(Some(kek.clone()))
                .run(arx, crx, etx),
        );
        assert!(matches!(erx.recv().await, Some(RecEvent::Started)));
        for _ in 0..5 {
            atx.send(RecFrame::Caller(vec![0x11; 320])).await.unwrap();
            atx.send(RecFrame::Bot(vec![0x22; 320])).await.unwrap();
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        drop(atx);
        let stats = h.await.unwrap().unwrap().unwrap();

        // The file on disk is an envelope, not a WAV.
        let raw = std::fs::read(&path).unwrap();
        assert_eq!(&raw[..8], b"SAIWAVA1");

        // Decrypting yields a well-formed WAV with patched sizes.
        let mut wav = Vec::new();
        crate::envelope::decrypt(std::io::Cursor::new(raw), &mut wav, &kek, false).unwrap();
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        let data = u32::from_le_bytes(wav[40..44].try_into().unwrap()) as u64;
        assert_eq!(data, stats.data_bytes);
        assert_eq!(wav.len() as u64, WAV_HEADER_LEN as u64 + data);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[tokio::test]
    async fn pause_omits_the_span() {
        let path = temp_path("pause");
        let (atx, arx) = mpsc::channel(64);
        let (ctx, crx) = mpsc::channel(8);
        let (etx, _erx) = mpsc::channel(8);
        let h = tokio::spawn(RecordingWriter::new(path.clone(), 8000, true).run(arx, crx, etx));
        // Record ~5 ticks, pause for ~5 ticks, resume for ~5 ticks.
        for _ in 0..5 {
            atx.send(RecFrame::Caller(vec![1u8; 320])).await.unwrap();
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        ctx.send(RecControl::Pause).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await; // ~5 ticks omitted
        ctx.send(RecControl::Resume).await.unwrap();
        for _ in 0..5 {
            atx.send(RecFrame::Caller(vec![1u8; 320])).await.unwrap();
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        drop(atx);
        let stats = h.await.unwrap().unwrap().unwrap();
        // The paused span produced no frames — total well under the ~15 ticks
        // of wall-clock the test spanned.
        assert!(
            stats.frames < 14,
            "paused span should be omitted, got {} frames",
            stats.frames
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
