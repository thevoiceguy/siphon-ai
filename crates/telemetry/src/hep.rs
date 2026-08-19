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
//! underlying `UdpHepSink` drops on a full queue and counts it; a
//! sampler task here mirrors that count — and the wire-send count —
//! into [`HEP_PACKETS_DROPPED_TOTAL`] and [`HEP_PACKETS_SENT_TOTAL`]
//! so operators see degradation without the call path stalling.
//!
//! **What the metrics cannot tell you.** `sent` counts wire-level
//! success, so a collector that is up but discarding still counts;
//! and an *unreachable* collector is counted nowhere — the send
//! failure is detected inside the upstream worker, which has no
//! counter for it, so those packets are neither `sent` nor
//! `dropped`. The signal for that case is the upstream throttled
//! `hep_rs::udp` WARN, which is why `hep_rs` must stay in the
//! daemon's default log filter (`bins/siphon-ai/src/main.rs`). A
//! `collector_up` gauge would need a send-failure counter added to
//! `hep-rs` — deliberately not faked here (siphon-ai #460).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::SystemTime;

use hep_rs::{
    HepPacket, HepProtocol, HepSinkHandle, IpProto, UdpHepSink, UdpHepSinkConfig, UdpHepSinkError,
};
use metrics::counter;
use thiserror::Error;
use tokio::task::JoinHandle;
use tracing::warn;

use crate::metrics::{HEP_PACKETS_DROPPED_TOTAL, HEP_PACKETS_SENT_TOTAL};

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
/// `Runtime` and drains it on shutdown — see
/// `bins/siphon-ai/src/runtime.rs::Runtime::run`. Keeping it
/// separate from [`HepTelemetry`] is what makes the latter
/// Arc-friendly.
pub struct HepWorkerHandle {
    /// A typed clone of the sink, kept solely to signal the worker's
    /// graceful drain. The share-by-Arc `HepSinkHandle` on
    /// [`HepTelemetry`] erases the concrete type, and `shutdown()` is a
    /// `UdpHepSink` method, so the worker owner holds its own clone.
    sink: UdpHepSink,
    worker: Option<JoinHandle<()>>,
    /// Periodic mirror of the sink's internal counters into the
    /// Prometheus registry. See [`sample_counters`].
    sampler: Option<JoinHandle<()>>,
}

/// How often [`sample_counters`] mirrors `hep-rs`'s atomics into the
/// metrics registry. HEP volume is a few packets per call, so the
/// series only needs to be fresher than a scrape interval; 10 s keeps
/// the task's cost to two atomic loads per tick while staying well
/// inside the shortest scrape anyone sensibly configures.
const HEP_METRICS_SAMPLE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(10);

/// Mirror `hep-rs`'s counters into the metrics registry.
///
/// The counts live upstream because SIP chunks are emitted by
/// `sip-hep` and RTCP/QoS by `forge-hep` — neither passes through this
/// crate, so there is no local call site to instrument. `absolute`
/// rather than `increment` because both upstream values are monotonic
/// totals: mirroring them directly cannot drift, whereas a delta
/// computed here would double-count on any missed tick.
fn publish_counters(sink: &UdpHepSink) {
    counter!(HEP_PACKETS_SENT_TOTAL).absolute(sink.sent());
    counter!(HEP_PACKETS_DROPPED_TOTAL, "reason" => "queue_full").absolute(sink.drops());
}

/// Republish [`publish_counters`] every
/// [`HEP_METRICS_SAMPLE_INTERVAL`] until cancelled.
async fn sample_counters(sink: UdpHepSink) {
    let mut ticker = tokio::time::interval(HEP_METRICS_SAMPLE_INTERVAL);
    // The first tick completes immediately; skip rather than burst if
    // the runtime ever stalls us past a whole interval.
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;
        publish_counters(&sink);
    }
}

/// How long to wait for the HEP worker to flush its queue on shutdown
/// before giving up and aborting. Generous enough for a realistic
/// end-of-drain backlog (a handful of CDR/QoS chunks) to reach a
/// responsive collector, bounded so a wedged or unreachable collector
/// can't hold up daemon exit.
const HEP_DRAIN_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

impl HepWorkerHandle {
    /// Drain the worker gracefully: signal it to stop accepting new
    /// packets and flush whatever is already queued, then await it
    /// within [`HEP_DRAIN_GRACE`]. Falls back to `abort()` only if the
    /// grace elapses (unreachable collector).
    ///
    /// Previously this aborted outright, discarding the queue — so a
    /// CDR/QoS chunk emitted right at shutdown (a drain-forced call's
    /// record) was lost even though the file CDR landed (siphon-ai
    /// #344). The global SIP/forge emitters hold `tx` clones forever, so
    /// the channel never closes on its own; `UdpHepSink::shutdown`
    /// closes the *receiver* instead, which drains regardless.
    pub async fn shutdown(mut self) {
        // Stop the periodic sampler first: the authoritative final
        // publish happens below, after the drain, and a tick landing
        // mid-drain would only be superseded.
        if let Some(sampler) = self.sampler.take() {
            sampler.abort();
        }
        let Some(worker) = self.worker.take() else {
            // Nothing to drain, but the counters may still have moved
            // since the last tick.
            publish_counters(&self.sink);
            return;
        };
        self.sink.shutdown();
        let drained = tokio::time::timeout(HEP_DRAIN_GRACE, worker).await;
        // Publish after the drain so the last packets — including the
        // drain-forced calls' CDR chunks, and any `send` that raced
        // the channel close and counted as a drop — are represented in
        // the final scrape rather than lost with the process.
        publish_counters(&self.sink);
        match drained {
            Ok(_) => {}
            Err(_) => {
                // The worker didn't finish flushing in time — almost
                // always a collector that isn't reading. Nothing more to
                // wait on; the JoinHandle is dropped, which detaches the
                // (now-closing) task rather than leaking it.
                warn!(
                    grace_ms = HEP_DRAIN_GRACE.as_millis(),
                    "HEP worker did not drain within grace; some queued chunks may be undelivered"
                );
            }
        }
    }
}

/// Inputs to [`HepTelemetry::build`]. Mirrors the fields of
/// `siphon-ai-config`'s `HepConfig` but accepts primitives so this
/// crate doesn't need to dep on `siphon-ai-config` (which would
/// close a cycle through `siphon-ai-core`).
/// The share-by-Arc `HepSink` handle the emitters and every sink leg
/// hold. Re-exported so the daemon can build its own legs (the SIP
/// ladder ring) without depending on `hep-rs` directly.
pub type SinkHandle = HepSinkHandle;

// No `Debug`/`Clone`: `extra_sinks` holds `dyn HepSink` legs, which
// are neither. The struct is consumed once by `build`.
pub struct HepTelemetryBuild {
    /// UDP collector to ship to. `None` builds no UDP leg and opens
    /// no socket — used when `[hep]` is off but a local consumer
    /// (the SIP ladder ring) still wants the packet stream. At least
    /// one of `collector` or `extra_sinks` must be present, or there
    /// is nothing to build.
    pub collector: Option<SocketAddr>,
    pub capture_id: u32,
    pub capture_password: Option<String>,
    pub queue_capacity: usize,
    pub node_id: String,
    /// Extra in-process `HepSink` legs fanned out alongside the UDP
    /// one — today just `sip_ring::SipRingSink`
    /// (DESIGN_SIP_LADDER.md §3.1). Teeing here rather than off the
    /// UDP sink is what lets SIP capture work on a node that ships
    /// nothing to Homer.
    pub extra_sinks: Vec<HepSinkHandle>,
}

impl HepTelemetry {
    /// Build a [`HepTelemetry`] from explicit fields. Returns the
    /// share-by-Arc handle plus the worker JoinHandle as a separate
    /// [`HepWorkerHandle`] — the runtime keeps the worker on
    /// `Runtime` and stashes the telemetry handle in `Arc` for
    /// admin / CDR / call-site consumers.
    /// The [`HepWorkerHandle`] is `None` when no `collector` was
    /// given — there is no UDP worker to drain in that case.
    pub async fn build(
        args: HepTelemetryBuild,
    ) -> Result<(Self, Option<HepWorkerHandle>), HepBuildError> {
        let HepTelemetryBuild {
            collector,
            capture_id,
            capture_password,
            queue_capacity,
            node_id,
            extra_sinks,
        } = args;

        // Build the UDP leg only when a collector is configured, so a
        // ring-only node opens no socket and spawns no worker.
        let mut legs: Vec<HepSinkHandle> = Vec::with_capacity(1 + extra_sinks.len());
        let mut worker_handle = None;
        if let Some(collector) = collector {
            let mut udp_cfg = UdpHepSinkConfig::new(collector);
            udp_cfg.queue_capacity = queue_capacity;
            let (sink, worker) = UdpHepSink::start(udp_cfg).await?;

            // A typed clone for the worker handle's graceful-drain
            // signal, taken before the sink is erased into the
            // share-by-Arc handle.
            let shutdown_sink = sink.clone();
            legs.push(Arc::new(sink) as HepSinkHandle);

            // Publish once before anything can be emitted, so both
            // series exist on `/metrics` from startup. The `metrics`
            // facade registers lazily — without this an alert on
            // `siphon_ai_hep_packets_dropped_total` would have to
            // survive the series being *absent* until the first drop,
            // which is exactly when nobody is looking at it.
            publish_counters(&shutdown_sink);
            let sampler = tokio::spawn(sample_counters(shutdown_sink.clone()));
            worker_handle = Some(HepWorkerHandle {
                sink: shutdown_sink,
                worker: Some(worker),
                sampler: Some(sampler),
            });
        }
        legs.extend(extra_sinks);

        // One leg stays one leg: the fan-out is only interposed when
        // there is genuinely more than one destination, so the common
        // HEP-only deployment keeps its exact previous call path.
        let arc_sink: HepSinkHandle = if legs.len() == 1 {
            legs.pop().expect("len checked")
        } else {
            Arc::new(crate::sip_ring::FanOutHepSink::new(legs))
        };

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

    // DESIGN_SIP_LADDER.md §3.1. The whole reason `collector` is an
    // Option: with `[hep]` off but the ladder on, the packet stream
    // must still exist — teeing off the UDP sink instead would leave
    // the ladder silently *empty* on such a node, which reads as "no
    // messages" rather than "not enabled".
    #[tokio::test]
    async fn build_without_a_collector_opens_no_socket_and_still_feeds_extra_sinks() {
        let capture = Arc::new(Capture::default());
        let (telemetry, worker) = HepTelemetry::build(HepTelemetryBuild {
            collector: None,
            capture_id: 0,
            capture_password: None,
            queue_capacity: 8,
            node_id: "ring-only".into(),
            extra_sinks: vec![capture.clone() as HepSinkHandle],
        })
        .await
        .expect("ring-only build succeeds");

        assert!(
            worker.is_none(),
            "no collector ⇒ no UDP worker to drain and no socket opened"
        );

        telemetry.emit_log("hello", Some("call-1"), None);
        let seen = capture.seen.lock().unwrap();
        assert_eq!(seen.len(), 1, "the extra leg still receives packets");
        assert_eq!(seen[0].correlation_id.as_deref(), Some("call-1"));
    }

    // With one destination the fan-out must not be interposed, so the
    // common HEP-only deployment keeps its exact previous call path.
    #[tokio::test]
    async fn a_single_leg_is_used_directly_rather_than_wrapped() {
        let capture = Arc::new(Capture::default());
        let (telemetry, worker) = HepTelemetry::build(HepTelemetryBuild {
            collector: None,
            capture_id: 7,
            capture_password: None,
            queue_capacity: 8,
            node_id: "one-leg".into(),
            extra_sinks: vec![capture.clone() as HepSinkHandle],
        })
        .await
        .expect("build");
        assert!(worker.is_none());
        assert!(
            Arc::ptr_eq(&telemetry.sink(), &(capture as HepSinkHandle)),
            "the sole leg is the sink itself, not a FanOut wrapper around it"
        );
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

    /// Render `/metrics` under a per-test recorder, the same way
    /// `crate::metrics`' own tests do.
    fn rendered<F: FnOnce()>(f: F) -> String {
        let recorder = crate::metrics::prometheus_builder()
            .expect("builder")
            .build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            crate::metrics::register_descriptions();
            f();
        });
        handle.render()
    }

    fn probe_packet() -> HepPacket {
        let zero = unspecified_addr();
        HepPacket {
            capture_id: 1,
            capture_password: None,
            protocol: HepProtocol::Log,
            transport: IpProto::Udp,
            src: zero,
            dst: zero,
            timestamp: SystemTime::now(),
            correlation_id: None,
            payload: b"probe".to_vec(),
        }
    }

    /// #460: both series must exist before the first packet, so an
    /// alert can be written against them directly instead of having to
    /// tolerate an absent series until something goes wrong.
    #[tokio::test]
    async fn counters_are_published_from_startup() {
        // Collector address is never listened on — nothing here needs
        // delivery, only the counters.
        let cfg = UdpHepSinkConfig::new("127.0.0.1:1".parse().unwrap());
        let (sink, _worker) = UdpHepSink::start(cfg).await.expect("sink starts");

        let out = rendered(|| publish_counters(&sink));

        assert!(
            out.contains("siphon_ai_hep_packets_sent_total 0"),
            "sent must be present and zero at startup; got:\n{out}"
        );
        assert!(
            out.contains(r#"siphon_ai_hep_packets_dropped_total{reason="queue_full"} 0"#),
            "queue_full drops must be present and zero at startup; got:\n{out}"
        );
        assert!(
            out.contains("# HELP siphon_ai_hep_packets_sent_total"),
            "HELP text must be registered; got:\n{out}"
        );
    }

    /// A full queue is the one failure these metrics genuinely
    /// observe, so pin that it reaches the registry rather than only
    /// hep-rs's private atomic.
    #[tokio::test]
    async fn queue_full_drops_reach_the_registry() {
        let mut cfg = UdpHepSinkConfig::new("127.0.0.1:1".parse().unwrap());
        cfg.queue_capacity = 1;
        let (sink, worker) = UdpHepSink::start(cfg).await.expect("sink starts");
        // Hold the worker off the queue so `try_send` has to hit a full
        // channel: never polled, so nothing is drained.
        drop(worker);

        for _ in 0..64 {
            sink.send(probe_packet());
        }

        assert!(sink.drops() > 0, "expected the bounded queue to overflow");
        let out = rendered(|| publish_counters(&sink));
        let expected = format!(
            r#"siphon_ai_hep_packets_dropped_total{{reason="queue_full"}} {}"#,
            sink.drops()
        );
        assert!(
            out.contains(&expected),
            "registry must mirror the sink's drop count exactly; \
             wanted {expected:?} in:\n{out}"
        );
    }
}
