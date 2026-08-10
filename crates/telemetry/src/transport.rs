//! Transport-layer metrics bridge.
//!
//! siphon-rs's `sip-transport` reports transport events through the
//! `sip_observe::TransportMetrics` trait, resolved via a process-global
//! that falls back to `NoopTransportMetrics` when nothing is installed.
//! siphon-ai installed nothing until 0.48.11, which is why the per-source
//! ingress rate limiter could drop SIP with no metric anywhere (#459) —
//! the drop happens in the transport, before the packet is parsed, so no
//! siphon-ai code path ever sees it.
//!
//! Only [`SIP_RATE_LIMITED_TOTAL`] is exported here. The trait's other
//! hooks (`on_packet_received`, `on_accept`, `on_latency`, …) duplicate
//! signal siphon-ai already derives at the call and dialog layers, where
//! it can be attributed to a route and a call; counting raw packets a
//! second time would add cardinality without adding an answer to any of
//! DEV_PLAN §11.8's questions. They are implemented as no-ops
//! deliberately, not by omission — see [`SiphonTransportMetrics`].

use metrics::counter;
use sip_observe::{OpLabel, StageLabel, TransportLabel, TransportMetrics};

use crate::metrics::SIP_RATE_LIMITED_TOTAL;

/// siphon-ai's `TransportMetrics`, installed once at startup by
/// [`install`].
///
/// Every hook except `on_rate_limited` is an intentional no-op: the
/// upstream trait is a firehose of per-packet events, and siphon-ai's
/// own instrumentation already covers the parts an operator asks about
/// with better attribution. Rate-limit drops are the exception because
/// nothing downstream can observe them — the packet is discarded before
/// it becomes a SIP message.
#[derive(Debug, Default)]
pub struct SiphonTransportMetrics;

impl TransportMetrics for SiphonTransportMetrics {
    fn on_rate_limited(&self, transport: TransportLabel) {
        counter!(SIP_RATE_LIMITED_TOTAL, "transport" => transport_label(transport)).increment(1);
    }

    fn on_packet_received(&self, _transport: TransportLabel) {}
    fn on_packet_sent(&self, _transport: TransportLabel) {}
    fn on_error(&self, _transport: TransportLabel, _stage: StageLabel) {}
    fn on_accept(&self, _transport: TransportLabel) {}
    fn on_connect(&self, _transport: TransportLabel) {}
    fn on_latency(&self, _transport: TransportLabel, _op: OpLabel, _nanos: u64) {}
}

/// Map the upstream label to a stable, lowercase metric label value.
///
/// Spelled out rather than derived from `Display` so a rename upstream
/// surfaces as a compile error here instead of silently renaming a
/// published label — the series is what alerts are written against.
fn transport_label(transport: TransportLabel) -> &'static str {
    match transport {
        TransportLabel::Udp => "udp",
        TransportLabel::Tcp => "tcp",
        TransportLabel::Tls => "tls",
        TransportLabel::Sctp => "sctp",
        TransportLabel::TlsSctp => "tls_sctp",
        TransportLabel::Ws => "ws",
        TransportLabel::Wss => "wss",
    }
}

/// Install siphon-ai's transport metrics as the process-global
/// implementation, and pre-register the drop counter at zero for every
/// transport so `/metrics` carries the series from startup.
///
/// Returns `false` if an implementation was already installed — the
/// upstream global is a `OnceCell`, so this is once-per-process. Callers
/// treat that as non-fatal: it happens only when two runtimes share a
/// process (tests), where the first install is as good as the second.
pub fn install() -> bool {
    // Zero-init before the first packet. Without this the series doesn't
    // exist until something is dropped, so an alert on it has to tolerate
    // `absent()` precisely when nobody is looking — the same trap the HEP
    // counters hit (#460).
    for transport in [
        TransportLabel::Udp,
        TransportLabel::Tcp,
        TransportLabel::Tls,
        TransportLabel::Ws,
        TransportLabel::Wss,
    ] {
        counter!(SIP_RATE_LIMITED_TOTAL, "transport" => transport_label(transport)).absolute(0);
    }
    sip_observe::set_transport_metrics(std::sync::Arc::new(SiphonTransportMetrics))
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn rate_limited_drops_are_counted_per_transport() {
        let out = rendered(|| {
            let m = SiphonTransportMetrics;
            m.on_rate_limited(TransportLabel::Udp);
            m.on_rate_limited(TransportLabel::Udp);
            m.on_rate_limited(TransportLabel::Tls);
        });
        assert!(
            out.contains(r#"siphon_ai_sip_rate_limited_total{transport="udp"} 2"#),
            "got:\n{out}"
        );
        assert!(
            out.contains(r#"siphon_ai_sip_rate_limited_total{transport="tls"} 1"#),
            "got:\n{out}"
        );
    }

    /// The other hooks fire on every packet; a stray `counter!` in one
    /// of them would be a hot-path regression that no other test covers.
    #[test]
    fn other_hooks_emit_nothing() {
        let out = rendered(|| {
            let m = SiphonTransportMetrics;
            m.on_packet_received(TransportLabel::Udp);
            m.on_packet_sent(TransportLabel::Udp);
            m.on_accept(TransportLabel::Tcp);
            m.on_connect(TransportLabel::Tcp);
            m.on_error(TransportLabel::Udp, StageLabel::FramingError);
            m.on_latency(TransportLabel::Udp, OpLabel::Recv, 42);
        });
        assert!(
            !out.contains("siphon_ai_sip_rate_limited_total"),
            "no counter should be touched by the no-op hooks; got:\n{out}"
        );
    }
}
