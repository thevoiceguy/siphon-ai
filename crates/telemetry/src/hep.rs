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
//!   SIP / RTCP view.
//! - `HepProtocol::Cdr` (0x65): the full CDR JSON when a call ends,
//!   same correlation key.
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
/// The daemon constructs this once at startup. Drop it on shutdown —
/// the underlying worker exits when the last clone of the sink
/// closes its mpsc channel.
pub struct HepTelemetry {
    sink: HepSinkHandle,
    capture_id: u32,
    capture_password: Option<String>,
    node_id: String,
    /// JoinHandle on the UDP worker task. Kept so callers can await
    /// shutdown deterministically; the worker exits automatically
    /// once every clone of the sink is dropped.
    worker: Option<JoinHandle<()>>,
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
    /// Build a [`HepTelemetry`] from explicit fields. The daemon's
    /// runtime calls this with the [`siphon-ai-config::HepConfig`]
    /// fields after compile-time validation.
    pub async fn build(args: HepTelemetryBuild) -> Result<Self, HepBuildError> {
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

        Ok(Self {
            sink: arc_sink,
            capture_id,
            capture_password,
            node_id,
            worker: Some(worker),
        })
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

    /// Emit a CDR JSON blob as chunk-type 101 (`Cdr`). Caller is
    /// responsible for serializing the CDR before invocation —
    /// telemetry doesn't pull the CDR schema in to avoid a dep on
    /// `siphon-ai-cdr`.
    pub fn emit_cdr_json(&self, json: &[u8], correlation_id: Option<&str>) {
        self.sink.send(HepPacket {
            capture_id: self.capture_id,
            capture_password: self.capture_password.clone(),
            protocol: HepProtocol::Cdr,
            transport: IpProto::Udp,
            src: unspecified_addr(),
            dst: unspecified_addr(),
            timestamp: SystemTime::now(),
            correlation_id: correlation_id.map(|s| s.to_string()),
            payload: json.to_vec(),
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

    /// Stop the worker on shutdown.
    ///
    /// We can't rely on dropping our local `Arc<dyn HepSink>` clone
    /// to close the producer channel — the per-protocol global
    /// emitters (`sip-hep` / `forge-hep`) hold their own clones in
    /// `OnceCell`s that don't get drained on daemon shutdown. So we
    /// abort the worker explicitly. Any packets queued at this
    /// moment are dropped, which is the right call: HEP is
    /// best-effort observability, and we'd rather finish shutting
    /// down than risk hanging on a wedged collector socket.
    pub async fn shutdown(mut self) {
        if let Some(worker) = self.worker.take() {
            worker.abort();
            // Bound the wait so a stuck abort can't block shutdown.
            // Abort yields a JoinError(Cancelled); we don't care.
            let _ = tokio::time::timeout(std::time::Duration::from_millis(250), worker).await;
        }
    }
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
