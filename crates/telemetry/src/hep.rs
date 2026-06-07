//! HEP3 (Homer) shipping for SiphonAI.
//!
//! Assembles a single [`hep_rs::UdpHepSink`] from `[hep]` config,
//! installs it as the global emitter for `sip-hep` (SIP signaling
//! capture inside siphon-rs) and `forge-hep` (RTCP + RTP-QoS inside
//! forge-media), and exposes a small SiphonAI-owned API for the
//! application-layer chunks Homer also renders:
//!
//! - `HepProtocol::Log` (0x64): one short text line per call lifecycle
//!   event (start, end, register state change). Carries the call_id
//!   as the correlation chunk so Homer threads it through the same
//!   SIP / RTCP view. See [`HepTelemetry::emit_log`].
//!
//! `HepProtocol::Cdr` (0x65) chunks — the full CDR JSON emitted when a
//! call ends — are composed by `siphon-ai-cdr`'s `HepCdrSink`, which
//! shares this module's `HepSink` via [`HepTelemetry::sink`] rather
//! than duplicating the packet-composition here.
//!
//! Per CLAUDE.md §4.7 emission is best-effort, never blocking. The
//! underlying `UdpHepSink` drops on full and the drop counter is
//! surfaced via metrics so operators see degradation without the
//! call path stalling.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::SystemTime;

use hep_rs::{
    HepPacket, HepProtocol, HepSinkHandle, IpProto, UdpHepSink, UdpHepSinkConfig, UdpHepSinkError,
};
use thiserror::Error;
use tokio::task::JoinHandle;

/// Telemetry-owned HEP plumbing. Holds the shared `Arc<dyn HepSink>`
/// for both the sip-hep / forge-hep emitters and SiphonAI's own
/// log/CDR emit calls.
///
/// Shape: `HepTelemetry` is the share-by-Arc handle that admin
/// endpoints, the CDR sink builder, and the call lifecycle all
/// borrow. The UDP worker `JoinHandle` is split out into
/// [`HepWorkerHandle`] so wrapping `HepTelemetry` in `Arc` doesn't
/// strand the worker on shutdown.
pub struct HepTelemetry {
    sink: HepSinkHandle,
    capture_id: u32,
    capture_password: Option<String>,
    node_id: String,
}

/// Owner of the spawned UDP worker. The runtime stashes this on
/// `Runtime` and aborts it on shutdown — see
/// `bins/siphon-ai/src/runtime.rs::Runtime::run`. Keeping it
/// separate from [`HepTelemetry`] is what makes the latter
/// Arc-friendly.
pub struct HepWorkerHandle {
    worker: Option<JoinHandle<()>>,
}

impl HepWorkerHandle {
    /// Abort the worker. Bounded wait so a wedged collector socket
    /// can't block daemon shutdown.
    pub async fn shutdown(mut self) {
        if let Some(worker) = self.worker.take() {
            worker.abort();
            let _ = tokio::time::timeout(std::time::Duration::from_millis(250), worker).await;
        }
    }
}

/// Inputs to [`HepTelemetry::build`]. Mirrors the fields of
/// `siphon-ai-config`'s `HepConfig` but accepts primitives so this
/// crate doesn't need to dep on `siphon-ai-config` (which would
/// close a cycle through `siphon-ai-core`).
#[derive(Debug, Clone)]
pub struct HepTelemetryBuild {
    pub collector: SocketAddr,
    pub capture_id: u32,
    pub capture_password: Option<String>,
    pub queue_capacity: usize,
    pub node_id: String,
}

impl HepTelemetry {
    /// Build a [`HepTelemetry`] from explicit fields. Returns the
    /// share-by-Arc handle plus the worker JoinHandle as a separate
    /// [`HepWorkerHandle`] — the runtime keeps the worker on
    /// `Runtime` and stashes the telemetry handle in `Arc` for
    /// admin / CDR / call-site consumers.
    pub async fn build(args: HepTelemetryBuild) -> Result<(Self, HepWorkerHandle), HepBuildError> {
        let HepTelemetryBuild {
            collector,
            capture_id,
            capture_password,
            queue_capacity,
            node_id,
        } = args;

        let mut udp_cfg = UdpHepSinkConfig::new(collector);
        udp_cfg.queue_capacity = queue_capacity;
        let (sink, worker) = UdpHepSink::start(udp_cfg).await?;

        let arc_sink: HepSinkHandle = Arc::new(sink);

        // Install the per-protocol emitters globally. siphon-rs's
        // `sip-transport` and forge-media's RTCP loop pick them up at
        // their hook sites. `set_emitter` is idempotent — second call
        // returns false; we ignore the result so multiple daemon
        // instances in one process (tests) don't trip the assert.
        let sip_emitter = sip_hep::SipHepEmitter::new(Arc::clone(&arc_sink), capture_id);
        let sip_emitter = match &capture_password {
            Some(pw) => sip_emitter.with_password(pw.clone()),
            None => sip_emitter,
        };
        let _ = sip_hep::set_emitter(Arc::new(sip_emitter));

        let forge_emitter = forge_hep::ForgeHepEmitter::new(Arc::clone(&arc_sink), capture_id);
        let forge_emitter = match &capture_password {
            Some(pw) => forge_emitter.with_password(pw.clone()),
            None => forge_emitter,
        };
        let _ = forge_hep::set_emitter(Arc::new(forge_emitter));

        let telemetry = Self {
            sink: arc_sink,
            capture_id,
            capture_password,
            node_id,
        };
        let worker_handle = HepWorkerHandle {
            worker: Some(worker),
        };
        Ok((telemetry, worker_handle))
    }

    /// Emit an application log line as a HEP3 chunk-type 100 (`Log`).
    /// Payload is the text verbatim. `peer_hint` is included as the
    /// HEP `dst` when set so Homer can render flows pointing at the
    /// right far-end host; both `src` and `dst` fall back to a
    /// synthetic `0.0.0.0:0` when the caller doesn't know.
    pub fn emit_log(
        &self,
        message: &str,
        correlation_id: Option<&str>,
        peer_hint: Option<SocketAddr>,
    ) {
        let src = peer_hint.unwrap_or_else(unspecified_addr);
        let dst = peer_hint.unwrap_or_else(unspecified_addr);
        self.sink.send(HepPacket {
            capture_id: self.capture_id,
            capture_password: self.capture_password.clone(),
            protocol: HepProtocol::Log,
            transport: IpProto::Udp,
            src,
            dst,
            timestamp: SystemTime::now(),
            correlation_id: correlation_id.map(|s| s.to_string()),
            payload: message.as_bytes().to_vec(),
        });
    }

    /// Emit a STIR/SHAKEN verdict as a HEP3 chunk-type 102
    /// (`HepProtocol::Verstat`). `payload` is the verdict already
    /// serialized (siphon-ai serializes the `VerificationResult` as JSON,
    /// the same shape as `start.verstat`); this crate stays free of the
    /// security types. `correlation_id` MUST be the SIP `Call-ID` so Homer
    /// threads the verdict onto the same call view as the SIP + RTCP + CDR
    /// chunks. Best-effort like every emit here — drops on a full queue.
    pub fn emit_verstat(&self, payload: &[u8], correlation_id: &str) {
        let zero = unspecified_addr();
        self.sink.send(HepPacket {
            capture_id: self.capture_id,
            capture_password: self.capture_password.clone(),
            protocol: HepProtocol::Verstat,
            transport: IpProto::Udp,
            src: zero,
            dst: zero,
            timestamp: SystemTime::now(),
            correlation_id: Some(correlation_id.to_string()),
            payload: payload.to_vec(),
        });
    }

    /// Node identifier the daemon was configured with (`[node].id`).
    /// Surfaced so loggers can prepend it to their text payloads
    /// without re-reading config.
    pub fn node_id(&self) -> &str {
        &self.node_id
    }

    /// Borrow the shared `HepSink` so downstream consumers (e.g., a
    /// `HepCdrSink` constructed by the daemon's CDR builder) can
    /// emit their own packet types using the same UDP worker.
    pub fn sink(&self) -> HepSinkHandle {
        Arc::clone(&self.sink)
    }

    /// Capture ID the emitters were built with. Surfaced so
    /// downstream `HepSink` users (CDR, log) can stamp the same
    /// `0x000C` chunk value on packets they emit directly.
    pub fn capture_id(&self) -> u32 {
        self.capture_id
    }

    /// HEPlify-Server shared password, if set. Surfaced for the
    /// same reason as [`Self::capture_id`].
    pub fn capture_password(&self) -> Option<&str> {
        self.capture_password.as_deref()
    }

    // Shutdown lives on [`HepWorkerHandle::shutdown`] now; the
    // telemetry handle itself is share-by-Arc and doesn't need a
    // teardown method.
}

/// Filled-in for callers that don't have a real `SocketAddr` handy.
/// HEP3 requires src/dst chunks; `0.0.0.0:0` is the conventional
/// placeholder used by Kamailio's `siptrace` and FreeSWITCH's
/// `mod_sofia` HEP for application-layer events.
fn unspecified_addr() -> SocketAddr {
    "0.0.0.0:0".parse().expect("static address parses")
}

/// Failure modes for [`HepTelemetry::build`].
#[derive(Debug, Error)]
pub enum HepBuildError {
    /// Failed to bind or connect the underlying UDP socket. Maps to
    /// the daemon's fail-on-startup behavior — a misconfigured
    /// collector address surfaces here.
    #[error(transparent)]
    Udp(#[from] UdpHepSinkError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use hep_rs::HepSink;
    use std::sync::Mutex;

    /// In-memory sink that records every packet, so tests can assert on
    /// the composed HEP3 shape without a real UDP collector.
    #[derive(Default)]
    struct Capture {
        seen: Mutex<Vec<HepPacket>>,
    }
    impl HepSink for Capture {
        fn send(&self, packet: HepPacket) {
            self.seen.lock().unwrap().push(packet);
        }
    }

    fn telemetry_with(sink: HepSinkHandle) -> HepTelemetry {
        HepTelemetry {
            sink,
            capture_id: 2002,
            capture_password: Some("homer-secret".into()),
            node_id: "node-a".into(),
        }
    }

    #[test]
    fn emit_verstat_composes_verstat_chunk_with_correlation() {
        let cap = Arc::new(Capture::default());
        let tel = telemetry_with(cap.clone() as HepSinkHandle);

        let payload = br#"{"attest":"A","signature_valid":true}"#;
        tel.emit_verstat(payload, "abc-123@pbx.example.com");

        let seen = cap.seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        let pkt = &seen[0];
        assert_eq!(pkt.protocol, HepProtocol::Verstat);
        assert_eq!(pkt.capture_id, 2002);
        assert_eq!(pkt.capture_password.as_deref(), Some("homer-secret"));
        // Correlation is the SIP Call-ID — the stitch into the call view.
        assert_eq!(
            pkt.correlation_id.as_deref(),
            Some("abc-123@pbx.example.com")
        );
        assert_eq!(pkt.payload, payload);
    }
}
