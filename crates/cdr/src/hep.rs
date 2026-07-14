//! `HepCdrSink` — ship completed CDRs to Homer as HEP3 chunk-type
//! 101 (`HepProtocol::Cdr`).
//!
//! Composes with the existing `MultiSink` pattern: the daemon
//! constructs a `HepCdrSink` when `[hep].enabled = true`, wraps it
//! in `Arc`, and adds it to the multiplexer alongside file / webhook
//! sinks. Per CLAUDE.md §4.7, emission is best-effort — the
//! underlying `HepSink` drops on full and surfaces drops via its own
//! counter; the CDR sink contract is fire-and-forget regardless.

use std::net::SocketAddr;
use std::time::SystemTime;

use async_trait::async_trait;
use hep_rs::{HepPacket, HepProtocol, HepSinkHandle, IpProto};
use tracing::warn;

use crate::schema::CdrRecord;
use crate::sink::CdrSink;

/// CDR sink that JSON-serializes each record into a HEP3 packet
/// (`HepProtocol::Cdr`, chunk type 101) and forwards it to a
/// [`HepSink`]. Correlation chunk is the SIP `call_id` so Homer
/// stitches the CDR onto the same call view as the SIP + RTCP
/// chunks already emitted by siphon-rs and forge-media.
pub struct HepCdrSink {
    sink: HepSinkHandle,
    capture_id: u32,
    capture_password: Option<String>,
}

impl HepCdrSink {
    /// Construct from a shared [`HepSink`]. Typically the daemon
    /// installs one `UdpHepSink` and clones the `Arc` here.
    pub fn new(sink: HepSinkHandle, capture_id: u32) -> Self {
        Self {
            sink,
            capture_id,
            capture_password: None,
        }
    }

    /// Set the HEPlify-Server shared password (chunk `0x000E`).
    pub fn with_password(mut self, password: impl Into<String>) -> Self {
        self.capture_password = Some(password.into());
        self
    }
}

#[async_trait]
impl CdrSink for HepCdrSink {
    async fn emit(&self, record: CdrRecord) {
        // serde_json::to_vec on the CDR schema is infallible for our
        // own types; the `match` guards against future schema changes
        // that could introduce a fallible field (e.g., non-string map
        // keys) without taking the call down on the error path.
        let payload = match serde_json::to_vec(&record) {
            Ok(bytes) => bytes,
            Err(e) => {
                warn!(error = %e, "failed to serialize CDR for HEP shipping; dropping");
                return;
            }
        };

        // Application-layer chunks don't have a meaningful network
        // 5-tuple. The 0.0.0.0:0 placeholder is the convention used
        // by Kamailio's `siptrace` and FreeSWITCH's `mod_sofia` HEP
        // for log / CDR chunks.
        let zero = "0.0.0.0:0".parse::<SocketAddr>().expect("static parses");

        self.sink.send(HepPacket {
            capture_id: self.capture_id,
            capture_password: self.capture_password.clone(),
            protocol: HepProtocol::Cdr,
            transport: IpProto::Udp,
            src: zero,
            dst: zero,
            timestamp: SystemTime::now(),
            correlation_id: Some(record.sip_call_id.clone()),
            payload,
        });
    }
}

impl std::fmt::Debug for HepCdrSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HepCdrSink")
            .field("capture_id", &self.capture_id)
            .field("has_password", &self.capture_password.is_some())
            .finish()
    }
}

// Tiny smoke test: build a sink against a capturing in-memory HepSink
// and assert the round-trip produces a Cdr packet with the SIP
// Call-ID as correlation.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{
        AudioInfo, CdrRecord, Direction, TerminationCause, TerminationInfo, CDR_VERSION,
    };
    use chrono::TimeZone;
    use hep_rs::HepSink;
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct Capture {
        seen: Mutex<Vec<HepPacket>>,
    }
    impl HepSink for Capture {
        fn send(&self, packet: HepPacket) {
            self.seen.lock().unwrap().push(packet);
        }
    }

    fn sample(call_id: &str, sip_call_id: &str) -> CdrRecord {
        CdrRecord {
            version: CDR_VERSION,
            call_id: call_id.into(),
            sip_call_id: sip_call_id.into(),
            started_at: chrono::Utc.with_ymd_and_hms(2026, 5, 5, 14, 30, 0).unwrap(),
            ended_at: chrono::Utc.with_ymd_and_hms(2026, 5, 5, 14, 30, 5).unwrap(),
            duration_ms: 5000,
            from: "+13125551212".into(),
            to: "5000".into(),
            direction: Direction::Inbound,
            route: "default".into(),
            ws_url: "wss://example/call".into(),
            audio: AudioInfo {
                codec: "PCMU".into(),
                payload_type: 0,
                sample_rate: 8000,
            },
            termination: TerminationInfo {
                cause: TerminationCause::ServerHangup,
                bridge_disconnect: "stop_sent".into(),
                tap_disconnect: "call_ended".into(),
            },
            verstat_attest: None,
            verstat_passed: None,
            recording_id: None,
            recording_path: None,
            recording_encrypted: None,
            recording_url: None,
            consent: None,
            park: None,
            hold: None,
            reconnect: None,
            quality: None,
        }
    }

    #[tokio::test]
    async fn round_trip_emits_cdr_chunk_with_correlation_id() {
        let cap = Arc::new(Capture::default());
        let sink =
            HepCdrSink::new(cap.clone() as HepSinkHandle, 2001).with_password("homer-secret");

        let rec = sample("siphon-1", "abc-123@pbx.example.com");
        sink.emit(rec.clone()).await;

        let seen = cap.seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        let pkt = &seen[0];
        assert_eq!(pkt.protocol, HepProtocol::Cdr);
        assert_eq!(pkt.capture_id, 2001);
        assert_eq!(pkt.capture_password.as_deref(), Some("homer-secret"));
        assert_eq!(
            pkt.correlation_id.as_deref(),
            Some("abc-123@pbx.example.com")
        );

        // Payload round-trips through JSON identically to the input.
        let decoded: CdrRecord = serde_json::from_slice(&pkt.payload).expect("json");
        assert_eq!(decoded.call_id, rec.call_id);
        assert_eq!(decoded.sip_call_id, rec.sip_call_id);
        assert_eq!(decoded.duration_ms, rec.duration_ms);
    }
}
