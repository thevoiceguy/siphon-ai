//! HEP3 (Homer) shipping for SiphonAI.
//!
//! Assembles a single [`hep_rs::UdpHepSink`] from `[hep]` config,
//! installs it as the global emitter for `sip-hep` (SIP signaling
//! capture inside siphon-rs) and `forge-hep` (RTCP + RTP-QoS inside
//! forge-media), and exposes a small SiphonAI-owned API for the
//! application-layer chunks Homer also renders:
//!
//! - `HepProtocol::Log` (0x64): one short text line when a bridged call
//!   starts and one when it ends, inbound and outbound alike. Carries
//!   the SIP Call-ID as the correlation chunk so Homer threads it onto
//!   the call's SIP ladder. See [`HepTelemetry::emit_call_lifecycle`];
//!   the admin `POST /admin/v1/hep/test` probe uses the lower-level
//!   [`HepTelemetry::emit_log`].
//!
//! - Node health, also as `Log` chunks: `node_started`, `node_ready`,
//!   `node_draining`, a periodic `node_status` and `node_stopping`, keyed
//!   by `node:<[node].id>` so Homer shows each node as its own timeline.
//!   See [`NodeHealthReporter`].
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
//! **An unreachable collector** (#596, closing the gap #460 left): the
//! sink also counts sends the socket refused
//! (`UdpHepSink::send_failures`), mirrored here as
//! `siphon_ai_hep_packets_dropped_total{reason="collector_down"}`, and
//! [`HEP_COLLECTOR_UP`] is derived from it each sample — `0` when any
//! send failed during the last interval, `1` otherwise. So every packet
//! handed to the sink is exactly one of sent / queue_full /
//! collector_down. The UDP caveat stands: a connected UDP socket only
//! learns a collector is dead from the ICMP port-unreachable it sends
//! back, so a *black-holed* collector (a firewall dropping silently)
//! still looks up — `sent` counts wire-level success, not delivery.
//! The throttled `hep_rs::udp` WARN remains, and the daemon's log
//! filter keeps a `warn` floor under every target so it is always
//! reachable (#597).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use hep_rs::{
    HepPacket, HepProtocol, HepSinkHandle, IpProto, UdpHepSink, UdpHepSinkConfig, UdpHepSinkError,
};
use metrics::counter;
use thiserror::Error;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;
use tracing::{debug, warn};

use crate::admin::{StatusFn, StatusResponse};
use crate::metrics::{
    HEP_COLLECTOR_UP, HEP_NODE_EVENTS_TOTAL, HEP_PACKETS_DROPPED_TOTAL, HEP_PACKETS_SENT_TOTAL,
};
use crate::readiness::ReadinessFlag;

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
    /// A UDP collector leg exists. False on a ring-only build, where
    /// the sink only feeds the in-process SIP ladder.
    ships_to_collector: bool,
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
fn publish_counters(sink: &UdpHepSink) -> u64 {
    counter!(HEP_PACKETS_SENT_TOTAL).absolute(sink.sent());
    counter!(HEP_PACKETS_DROPPED_TOTAL, "reason" => "queue_full").absolute(sink.drops());
    let failures = sink.send_failures();
    counter!(HEP_PACKETS_DROPPED_TOTAL, "reason" => "collector_down").absolute(failures);
    failures
}

/// Publish the collector-up gauge from two consecutive send-failure
/// readings: up unless a send failed in between (#596). Derived over a
/// window rather than from the last send because a connected UDP socket
/// reports a dead collector's ICMP refusal on the *next* send, so
/// individual sends alternate success/failure and a point sample would
/// flap.
fn publish_collector_up(previous_failures: u64, failures: u64) {
    let up = if failures > previous_failures {
        0.0
    } else {
        1.0
    };
    metrics::gauge!(HEP_COLLECTOR_UP).set(up);
}

/// Republish [`publish_counters`] every
/// [`HEP_METRICS_SAMPLE_INTERVAL`] until cancelled.
async fn sample_counters(sink: UdpHepSink, mut previous_failures: u64) {
    let mut ticker = tokio::time::interval(HEP_METRICS_SAMPLE_INTERVAL);
    // The first tick completes immediately; skip rather than burst if
    // the runtime ever stalls us past a whole interval.
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;
        let failures = publish_counters(&sink);
        publish_collector_up(previous_failures, failures);
        previous_failures = failures;
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
            let failures = publish_counters(&shutdown_sink);
            // Presumed reachable until a send says otherwise; the first
            // sample interval settles it.
            publish_collector_up(failures, failures);
            let sampler = tokio::spawn(sample_counters(shutdown_sink.clone(), failures));
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
            ships_to_collector: collector.is_some(),
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

    /// Emit one call lifecycle moment as a `Log` chunk correlated by the
    /// SIP Call-ID, so it lands on the same Homer call view as the SIP
    /// ladder and the CDR (#604). `call_id` is the bridge id
    /// (`siphon-…`), carried in the text so the line can be joined to the
    /// CDR, webhooks, and daemon logs; `direction` is `inbound` or
    /// `outbound`.
    ///
    /// The payload is one `key=value` line led by the event name —
    /// `call_started call_id=… direction=… node=… route=… from=… to=…`
    /// or `call_ended call_id=… direction=… node=… cause=… duration_ms=…`
    /// — with `cause` the CDR's `termination.cause` label. Values that
    /// are not a single plain token are quoted and escaped: `from`/`to`
    /// come off the wire, and an embedded newline must not forge a second
    /// line in the collector.
    pub fn emit_call_lifecycle(
        &self,
        sip_call_id: &str,
        call_id: &str,
        direction: &str,
        event: CallLifecycle<'_>,
    ) {
        self.emit_log(
            &call_lifecycle_line(&self.node_id, call_id, direction, event),
            Some(sip_call_id),
            None,
        );
    }

    /// Emit one node-health moment as a `Log` chunk correlated by
    /// [`node_correlation_id`] (`node:<[node].id>`), so Homer shows a
    /// node's health as its own timeline, apart from any call. The
    /// payload is one `key=value` line led by the event name —
    /// `node_status node=… version=… ready=… draining=… active_calls=…
    /// registrations=<registered>/<total> uptime_secs=…` — built from the
    /// `GET /admin/v1/status` snapshot plus the `/ready` flag.
    /// [`NodeHealthReporter`] is the caller in the daemon.
    pub fn emit_node_health(&self, event: NodeEvent, status: &StatusResponse, ready: bool) {
        self.emit_log(
            &node_health_line(&self.node_id, event, status, ready),
            Some(&node_correlation_id(&self.node_id)),
            None,
        );
        counter!(HEP_NODE_EVENTS_TOTAL, "event" => event.as_str()).increment(1);
        debug!(
            event = event.as_str(),
            ready,
            draining = status.draining,
            active_calls = status.active_calls,
            "HEP node-health chunk queued"
        );
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

    /// Whether anything leaves the process. `false` on a ring-only
    /// build (`[hep]` off, SIP ladder on): the plumbing exists so the
    /// ladder can capture, but there is no collector. Callers that
    /// mean "HEP is configured" must ask this, not whether a
    /// `HepTelemetry` exists.
    pub fn ships_to_collector(&self) -> bool {
        self.ships_to_collector
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

/// The call lifecycle moments [`HepTelemetry::emit_call_lifecycle`]
/// ships — the timeline `docs/HEP.md` describes under *What appears in
/// Homer's UI*.
#[derive(Debug, Clone, Copy)]
pub enum CallLifecycle<'a> {
    /// The call is bridged and its controller is starting. `route` is
    /// the matched route (inbound) or gateway (outbound) — the CDR's
    /// `route` field.
    Started {
        route: &'a str,
        from: &'a str,
        to: &'a str,
    },
    /// The call has been torn down. `cause` is the CDR
    /// `termination.cause` label; `duration_ms` the CDR's duration.
    Ended { cause: &'a str, duration_ms: u64 },
}

/// Compose the text payload for [`HepTelemetry::emit_call_lifecycle`].
fn call_lifecycle_line(
    node_id: &str,
    call_id: &str,
    direction: &str,
    event: CallLifecycle<'_>,
) -> String {
    let mut line = String::with_capacity(160);
    line.push_str(match event {
        CallLifecycle::Started { .. } => "call_started",
        CallLifecycle::Ended { .. } => "call_ended",
    });
    push_field(&mut line, "call_id", call_id);
    push_field(&mut line, "direction", direction);
    push_field(&mut line, "node", node_id);
    match event {
        CallLifecycle::Started { route, from, to } => {
            push_field(&mut line, "route", route);
            push_field(&mut line, "from", from);
            push_field(&mut line, "to", to);
        }
        CallLifecycle::Ended { cause, duration_ms } => {
            push_field(&mut line, "cause", cause);
            push_field(&mut line, "duration_ms", &duration_ms.to_string());
        }
    }
    line
}

/// Append ` key=value`, quoting (`{:?}`) any value that isn't one plain
/// printable-ASCII token, so the line stays unambiguous to split.
fn push_field(line: &mut String, key: &str, value: &str) {
    use std::fmt::Write as _;
    line.push(' ');
    line.push_str(key);
    line.push('=');
    let plain = !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_graphic() && b != b'"' && b != b'=');
    if plain {
        line.push_str(value);
    } else {
        // Writing to a String cannot fail.
        let _ = write!(line, "{value:?}");
    }
}

/// The node-health moments [`HepTelemetry::emit_node_health`] ships — see
/// `docs/HEP.md` → *Node health Log chunks*.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeEvent {
    /// The reporter started: the first line a node sends after boot.
    Started,
    /// `/ready` went 503 → 200.
    Ready,
    /// `/ready` went 200 → 503 without a drain. Nothing does that today;
    /// if something starts to, Homer shows it rather than hiding it.
    NotReady,
    /// A graceful drain began (SIGTERM or `POST /admin/v1/drain`).
    Draining,
    /// The periodic snapshot, every `[hep].node_status_interval_secs`.
    Status,
    /// Teardown — the last line, queued before the HEP worker drains.
    Stopping,
}

impl NodeEvent {
    /// The line's leading token, and the `event` label on
    /// `siphon_ai_hep_node_events_total`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Started => "node_started",
            Self::Ready => "node_ready",
            Self::NotReady => "node_not_ready",
            Self::Draining => "node_draining",
            Self::Status => "node_status",
            Self::Stopping => "node_stopping",
        }
    }
}

/// Correlation id for a node's health chunks: `node:<[node].id>`. Not a
/// SIP Call-ID, and namespaced so it can never be mistaken for one.
pub fn node_correlation_id(node_id: &str) -> String {
    format!("node:{node_id}")
}

/// Compose the text payload for [`HepTelemetry::emit_node_health`].
fn node_health_line(
    node_id: &str,
    event: NodeEvent,
    status: &StatusResponse,
    ready: bool,
) -> String {
    let flag = |b: bool| if b { "true" } else { "false" };
    let mut line = String::with_capacity(160);
    line.push_str(event.as_str());
    push_field(&mut line, "node", node_id);
    push_field(&mut line, "version", &status.version);
    push_field(&mut line, "ready", flag(ready));
    push_field(&mut line, "draining", flag(status.draining));
    push_field(&mut line, "active_calls", &status.active_calls.to_string());
    push_field(
        &mut line,
        "registrations",
        &format!(
            "{}/{}",
            status.registrations.registered, status.registrations.total
        ),
    );
    push_field(&mut line, "uptime_secs", &status.uptime_secs.to_string());
    line
}

/// The event a change in `(ready, draining)` amounts to, if any. A drain
/// flips both at once and reads as `node_draining` alone: not-ready is
/// implied by it, and reporting both would say the same thing twice.
fn node_transition(before: (bool, bool), now: (bool, bool)) -> Option<NodeEvent> {
    let ((was_ready, was_draining), (ready, draining)) = (before, now);
    if draining && !was_draining {
        Some(NodeEvent::Draining)
    } else if ready && !was_ready {
        Some(NodeEvent::Ready)
    } else if was_ready && !ready && !draining {
        Some(NodeEvent::NotReady)
    } else {
        None
    }
}

/// How often [`NodeHealthReporter`] checks `/ready` and the drain flag for
/// a change: well inside any probe period, and each check is an atomic
/// load plus the status snapshot.
const NODE_HEALTH_POLL: Duration = Duration::from_secs(1);

/// How long [`NodeHealthReporter::stop`] waits for the last lines before
/// abandoning the task rather than holding up teardown.
const NODE_HEALTH_STOP_GRACE: Duration = Duration::from_secs(1);

/// Ships a node's health to Homer for the life of the daemon: a
/// `node_started` line when spawned, `node_ready` / `node_not_ready` /
/// `node_draining` when `/ready` or the drain flag changes (checked every
/// [`NODE_HEALTH_POLL`]), a `node_status` snapshot every heartbeat, and a
/// final `node_stopping` from [`Self::stop`].
///
/// The state is read, never held: `status` is the closure
/// `GET /admin/v1/status` serves and `readiness` the flag `/ready` answers
/// from, so Homer, the admin API and a load balancer's probe cannot
/// disagree. Every emit is a non-blocking queue push (CLAUDE.md §4.7).
pub struct NodeHealthReporter {
    stop_tx: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
}

impl NodeHealthReporter {
    /// Start reporting. `heartbeat` is the `node_status` period; `None`
    /// sends the transitions only. Must be called inside a Tokio runtime.
    pub fn spawn(
        hep: Arc<HepTelemetry>,
        status: StatusFn,
        readiness: ReadinessFlag,
        heartbeat: Option<Duration>,
    ) -> Self {
        Self::spawn_polling(hep, status, readiness, heartbeat, NODE_HEALTH_POLL)
    }

    fn spawn_polling(
        hep: Arc<HepTelemetry>,
        status: StatusFn,
        readiness: ReadinessFlag,
        heartbeat: Option<Duration>,
        poll: Duration,
    ) -> Self {
        // `node_started` goes out here, synchronously, not from the task:
        // the runtime flips `/ready` right after spawning, and a task that
        // first ran after the flip would report `ready=true` with no
        // `node_ready` to follow it.
        let (st, ready) = (status(), readiness.is_ready());
        hep.emit_node_health(NodeEvent::Started, &st, ready);
        let last = (ready, st.draining);
        let (stop_tx, stop_rx) = oneshot::channel();
        let task = tokio::spawn(report_node_health(
            hep, status, readiness, heartbeat, poll, last, stop_rx,
        ));
        Self {
            stop_tx: Some(stop_tx),
            task: Some(task),
        }
    }

    /// Stop reporting: one last check — a drain shorter than a poll would
    /// otherwise go unreported — then `node_stopping`. Call before the HEP
    /// worker drains so both reach the wire. Bounded by
    /// [`NODE_HEALTH_STOP_GRACE`]; a task that overruns it is aborted
    /// rather than holding up teardown.
    pub async fn stop(mut self) {
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.send(());
        }
        if let Some(mut task) = self.task.take() {
            if tokio::time::timeout(NODE_HEALTH_STOP_GRACE, &mut task)
                .await
                .is_err()
            {
                task.abort();
                warn!(
                    grace_ms = NODE_HEALTH_STOP_GRACE.as_millis(),
                    "HEP node-health reporter did not stop within grace; its last lines may be missing"
                );
            }
        }
    }
}

impl Drop for NodeHealthReporter {
    /// A reporter dropped without [`Self::stop`] — a startup error path, a
    /// test — must not outlive its owner.
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

async fn report_node_health(
    hep: Arc<HepTelemetry>,
    status: StatusFn,
    readiness: ReadinessFlag,
    heartbeat: Option<Duration>,
    poll: Duration,
    mut last: (bool, bool),
    mut stop_rx: oneshot::Receiver<()>,
) {
    let snapshot = || (status(), readiness.is_ready());

    let mut poll_tick = tokio::time::interval(poll);
    poll_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    // The first tick completes immediately; `node_started` just covered it.
    poll_tick.tick().await;
    let mut beat = heartbeat.map(|period| {
        let mut beat = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
        beat.set_missed_tick_behavior(MissedTickBehavior::Skip);
        beat
    });

    loop {
        tokio::select! {
            // Sent by `stop`; an `Err` (sender dropped) means the same.
            _ = &mut stop_rx => break,
            _ = poll_tick.tick() => {
                let (st, ready) = snapshot();
                let now = (ready, st.draining);
                if let Some(event) = node_transition(last, now) {
                    hep.emit_node_health(event, &st, ready);
                }
                last = now;
            }
            _ = next_beat(&mut beat) => {
                let (st, ready) = snapshot();
                hep.emit_node_health(NodeEvent::Status, &st, ready);
            }
        }
    }

    let (st, ready) = snapshot();
    if let Some(event) = node_transition(last, (ready, st.draining)) {
        hep.emit_node_health(event, &st, ready);
    }
    hep.emit_node_health(NodeEvent::Stopping, &st, ready);
}

/// The next heartbeat tick, or never when the heartbeat is off.
async fn next_beat(beat: &mut Option<tokio::time::Interval>) {
    match beat {
        Some(beat) => {
            beat.tick().await;
        }
        None => std::future::pending::<()>().await,
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

#[cfg(test)]
mod tests {
    use super::*;
    use hep_rs::HepSink;
    use std::sync::atomic::{AtomicBool, Ordering};
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

    #[tokio::test]
    async fn build_with_a_collector_reports_that_it_ships() {
        let homer = tokio::net::UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("bind fake collector");
        let (telemetry, worker) = HepTelemetry::build(HepTelemetryBuild {
            collector: Some(homer.local_addr().expect("local addr")),
            capture_id: 7,
            capture_password: None,
            queue_capacity: 8,
            node_id: "shipping".into(),
            extra_sinks: Vec::new(),
        })
        .await
        .expect("collector build succeeds");
        assert!(telemetry.ships_to_collector());
        assert!(worker.is_some());
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
        assert!(
            !telemetry.ships_to_collector(),
            "ring-only must not read as HEP enabled (status, CDR sink)"
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
            ships_to_collector: true,
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

    // #604: lifecycle chunks correlate by SIP Call-ID (the key Homer's
    // call view threads on), not the bridge id — which rides the text.
    #[test]
    fn call_lifecycle_chunks_correlate_by_sip_call_id() {
        let cap = Arc::new(Capture::default());
        let tel = telemetry_with(cap.clone() as HepSinkHandle);

        tel.emit_call_lifecycle(
            "abc-123@pbx.example.com",
            "siphon-4380c265",
            "inbound",
            CallLifecycle::Started {
                route: "main_reception",
                from: "+13125551234",
                to: "5000",
            },
        );
        tel.emit_call_lifecycle(
            "abc-123@pbx.example.com",
            "siphon-4380c265",
            "inbound",
            CallLifecycle::Ended {
                cause: "remote_bye",
                duration_ms: 110_234,
            },
        );

        let seen = cap.seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        for pkt in seen.iter() {
            assert_eq!(pkt.protocol, HepProtocol::Log);
            assert_eq!(pkt.capture_id, 2002);
            assert_eq!(pkt.capture_password.as_deref(), Some("homer-secret"));
            assert_eq!(
                pkt.correlation_id.as_deref(),
                Some("abc-123@pbx.example.com")
            );
        }
        assert_eq!(
            std::str::from_utf8(&seen[0].payload).unwrap(),
            "call_started call_id=siphon-4380c265 direction=inbound node=node-a \
             route=main_reception from=+13125551234 to=5000"
        );
        assert_eq!(
            std::str::from_utf8(&seen[1].payload).unwrap(),
            "call_ended call_id=siphon-4380c265 direction=inbound node=node-a \
             cause=remote_bye duration_ms=110234"
        );
    }

    // `from`/`to` are caller-controlled: a newline must not forge a
    // second line, and a space or `=` must not forge a field.
    #[test]
    fn lifecycle_values_that_are_not_plain_tokens_are_quoted() {
        let line = call_lifecycle_line(
            "node-a",
            "siphon-1",
            "inbound",
            CallLifecycle::Started {
                route: "r",
                from: "Alice Smith <sip:a@b>",
                to: "x\ncall_ended cause=forged",
            },
        );
        assert_eq!(
            line,
            "call_started call_id=siphon-1 direction=inbound node=node-a route=r \
             from=\"Alice Smith <sip:a@b>\" to=\"x\\ncall_ended cause=forged\""
        );
        assert!(!line.contains('\n'));
        // An empty value still reads as a present-but-empty field.
        let empty = call_lifecycle_line(
            "",
            "siphon-1",
            "outbound",
            CallLifecycle::Ended {
                cause: "local_shutdown",
                duration_ms: 0,
            },
        );
        assert!(empty.contains(" node=\"\" "), "{empty}");
    }

    fn node_status(draining: bool, active_calls: usize) -> StatusResponse {
        StatusResponse {
            version: "9.9.9".into(),
            uptime_secs: 42,
            active_calls,
            registrations: crate::admin::RegistrationsSummary {
                registered: 1,
                total: 2,
            },
            draining,
            hep_enabled: true,
        }
    }

    /// Every `Log` chunk the capture saw, as (payload, correlation id).
    fn log_lines(cap: &Capture) -> Vec<(String, Option<String>)> {
        cap.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|p| p.protocol == HepProtocol::Log)
            .map(|p| {
                (
                    String::from_utf8(p.payload.clone()).unwrap(),
                    p.correlation_id.clone(),
                )
            })
            .collect()
    }

    // Node health is keyed by the node, not a call — and namespaced so it
    // can never collide with a SIP Call-ID in Homer's search.
    #[test]
    fn node_health_chunk_is_keyed_by_node_not_a_call() {
        let cap = Arc::new(Capture::default());
        let tel = telemetry_with(cap.clone() as HepSinkHandle);

        tel.emit_node_health(NodeEvent::Status, &node_status(false, 3), true);

        let lines = log_lines(&cap);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].1.as_deref(), Some("node:node-a"));
        assert_eq!(
            lines[0].0,
            "node_status node=node-a version=9.9.9 ready=true draining=false \
             active_calls=3 registrations=1/2 uptime_secs=42"
        );
    }

    #[test]
    fn a_drain_reads_as_draining_alone() {
        // (ready, draining) before → after.
        assert_eq!(
            node_transition((false, false), (true, false)),
            Some(NodeEvent::Ready)
        );
        assert_eq!(
            node_transition((true, false), (false, true)),
            Some(NodeEvent::Draining),
            "a drain flips both; it must not also read as not-ready"
        );
        assert_eq!(
            node_transition((true, false), (false, false)),
            Some(NodeEvent::NotReady)
        );
        assert_eq!(node_transition((true, false), (true, false)), None);
        assert_eq!(node_transition((false, true), (false, true)), None);
    }

    /// The first token of each `Log` line, asserting every one is keyed by
    /// the node.
    fn node_events(cap: &Capture) -> Vec<String> {
        log_lines(cap)
            .into_iter()
            .map(|(line, correlation)| {
                assert_eq!(correlation.as_deref(), Some("node:node-a"), "{line}");
                line.split(' ').next().unwrap_or_default().to_string()
            })
            .collect()
    }

    #[tokio::test]
    async fn reporter_ships_start_ready_heartbeat_draining_and_stopping() {
        let cap = Arc::new(Capture::default());
        let tel = Arc::new(telemetry_with(cap.clone() as HepSinkHandle));
        let draining = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&draining);
        let status: StatusFn = Arc::new(move || node_status(flag.load(Ordering::SeqCst), 0));
        let readiness = ReadinessFlag::new();
        let reporter = NodeHealthReporter::spawn_polling(
            tel,
            status,
            readiness.clone(),
            Some(Duration::from_millis(60)),
            Duration::from_millis(10),
        );

        tokio::time::sleep(Duration::from_millis(30)).await;
        readiness.mark_ready();
        tokio::time::sleep(Duration::from_millis(120)).await;
        // A drain faster than a poll: `stop` must still report it.
        draining.store(true, Ordering::SeqCst);
        readiness.mark_not_ready();
        reporter.stop().await;

        let events = node_events(&cap);
        assert_eq!(
            events.first().map(String::as_str),
            Some("node_started"),
            "{events:?}"
        );
        assert_eq!(
            events.last().map(String::as_str),
            Some("node_stopping"),
            "{events:?}"
        );
        let count = |e: &str| events.iter().filter(|x| *x == e).count();
        assert_eq!(count("node_ready"), 1, "{events:?}");
        assert_eq!(count("node_draining"), 1, "{events:?}");
        assert_eq!(
            count("node_not_ready"),
            0,
            "a drain is not not-ready: {events:?}"
        );
        assert!(count("node_status") >= 1, "a heartbeat fired: {events:?}");
        let at = |e: &str| events.iter().position(|x| x == e);
        assert!(at("node_ready") < at("node_draining"), "{events:?}");
    }

    #[tokio::test]
    async fn heartbeat_off_sends_transitions_only() {
        let cap = Arc::new(Capture::default());
        let tel = Arc::new(telemetry_with(cap.clone() as HepSinkHandle));
        let status: StatusFn = Arc::new(|| node_status(false, 0));
        let reporter = NodeHealthReporter::spawn_polling(
            tel,
            status,
            ReadinessFlag::new(),
            None,
            Duration::from_millis(10),
        );

        tokio::time::sleep(Duration::from_millis(80)).await;
        reporter.stop().await;

        assert_eq!(node_events(&cap), ["node_started", "node_stopping"]);
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
    /// #596: the gauge is derived over a sample window, so the
    /// alternating success/failure a connected UDP socket produces
    /// against a dead collector reads as a steady 0, and a quiet
    /// interval after recovery reads as 1.
    #[test]
    fn collector_up_follows_send_failures_over_the_window() {
        let out = rendered(|| publish_collector_up(0, 0));
        assert!(out.contains("siphon_ai_hep_collector_up 1"), "{out}");
        let out = rendered(|| publish_collector_up(3, 7));
        assert!(out.contains("siphon_ai_hep_collector_up 0"), "{out}");
        let out = rendered(|| publish_collector_up(7, 7));
        assert!(out.contains("siphon_ai_hep_collector_up 1"), "{out}");
    }

    #[tokio::test]
    async fn counters_are_published_from_startup() {
        // Collector address is never listened on — nothing here needs
        // delivery, only the counters.
        let cfg = UdpHepSinkConfig::new("127.0.0.1:1".parse().unwrap());
        let (sink, _worker) = UdpHepSink::start(cfg).await.expect("sink starts");

        let out = rendered(|| {
            publish_counters(&sink);
        });

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
        let out = rendered(|| {
            publish_counters(&sink);
        });
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
