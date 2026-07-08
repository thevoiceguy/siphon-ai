//! End-to-end test of `siphon-ai decrypt-recording` (0.24.0): seal a WAV
//! through the recording writer exactly as the daemon does, then decrypt it
//! back with the real binary.

use std::path::{Path, PathBuf};
use std::process::Command;

use siphon_ai_recording::{Kek, RecControl, RecEvent, RecFrame, RecordingWriter};
use tokio::sync::mpsc;

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("siphon_cli_dec_{}_{}", std::process::id(), tag));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

const KEK_HEX: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

fn kek() -> Kek {
    Kek::from_hex(KEK_HEX, "cli-test-key".into()).unwrap()
}

/// Record a short encrypted call and return the `.wava` path.
async fn record_encrypted(dir: &Path) -> PathBuf {
    let path = dir.join("call.wava");
    let (atx, arx) = mpsc::channel(64);
    let (_ctx, crx) = mpsc::channel::<RecControl>(8);
    let (etx, mut erx) = mpsc::channel(8);
    let task = tokio::spawn(
        RecordingWriter::new(path.clone(), 8000, true)
            .with_encryption(Some(kek()))
            .run(arx, crx, etx),
    );
    assert!(matches!(erx.recv().await, Some(RecEvent::Started)));
    for _ in 0..5 {
        atx.send(RecFrame::Caller(vec![0x0F; 320])).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    drop(atx);
    task.await.unwrap().unwrap().unwrap();
    path
}

#[tokio::test]
async fn decrypt_recording_cli_roundtrip() {
    let dir = temp_dir("roundtrip");
    let wava = record_encrypted(&dir).await;
    let kek_file = dir.join("kek.hex");
    std::fs::write(&kek_file, format!("{KEK_HEX}\n")).unwrap();

    let out = dir.join("out.wav");
    let status = Command::new(env!("CARGO_BIN_EXE_siphon-ai"))
        .args([
            "decrypt-recording",
            wava.to_str().unwrap(),
            "--kek-file",
            kek_file.to_str().unwrap(),
            "--out",
            out.to_str().unwrap(),
        ])
        .status()
        .expect("run siphon-ai decrypt-recording");
    assert!(status.success(), "decrypt-recording must exit 0");

    let wav = std::fs::read(&out).unwrap();
    assert_eq!(&wav[0..4], b"RIFF");
    assert_eq!(&wav[8..12], b"WAVE");
    let data = u32::from_le_bytes(wav[40..44].try_into().unwrap()) as usize;
    assert_eq!(wav.len(), 44 + data, "header sizes must be patched");
    assert!(data > 0, "payload must be non-empty");

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn decrypt_recording_cli_wrong_key_fails() {
    let dir = temp_dir("wrongkey");
    let wava = record_encrypted(&dir).await;
    let kek_file = dir.join("kek.hex");
    std::fs::write(&kek_file, "ff".repeat(32)).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_siphon-ai"))
        .args([
            "decrypt-recording",
            wava.to_str().unwrap(),
            "--kek-file",
            kek_file.to_str().unwrap(),
        ])
        .output()
        .expect("run siphon-ai decrypt-recording");
    assert!(!output.status.success(), "wrong KEK must exit non-zero");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("cli-test-key"),
        "error must name the key_id the recording needs, got: {stderr}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
