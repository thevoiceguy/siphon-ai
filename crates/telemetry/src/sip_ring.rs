//! Per-call SIP ladder ring (DESIGN_SIP_LADDER.md).
//!
//! Holds the recent SIP messages for each call, keyed by SIP
//! `Call-ID`, and serves them on `GET /admin/v1/calls/{id}/sip` so an
//! operator can see a call's signaling without leaving sightglass.
//!
//! **This is a nice-to-have, not a Homer replacement** (design §0).
//! Minutes of history for one node, no search, no cross-node
//! correlation. Anything in depth belongs in HEP/Homer.
//!
//! Fed by [`SipRingSink`], a `HepSink` leg in the fan-out the runtime
//! builds — so capture needs no siphon-rs change and no second parse
//! of the wire: siphon-rs's `sip-hep` emitter already hands every SIP
//! message to that sink as a `HepPacket` carrying the raw bytes, the
//! `Call-ID` as correlation id, `src`/`dst` and a timestamp.
//!
//! Process-global for the same reason `cdr_ring` and `error_ring`
//! are: the capturing sink is rebuilt on SIGHUP reloads while the
//! ring — and its configured capacity — survives.
//!
//! ## Not every SIP dialog is a call
//!
//! The design note's §3.2 assumed two bounds (per-call messages, and
//! completed calls retained). Implementation found a third case it
//! did not account for, and the ring would have grown without bound
//! without it: **the SIP stream carries far more than calls.**
//! REGISTER refreshes, OPTIONS pings, unsolicited NOTIFYs and — on any
//! public-IP node — a steady drip of scanner INVITEs rejected with 403
//! all carry a `Call-ID` and all reach this sink. None of them ever
//! becomes a call, so none ever emits a CDR, so none would ever be
//! *completed* and evicted. The reference node sees ~1,440 REGISTER
//! cycles and ~150 rejected INVITEs a day.
//!
//! So traces live in three populations:
//!
//! - **completed** — a CDR was emitted for them (the runtime calls
//!   [`complete`] from its ring CDR sink). Capped at `cap_calls`,
//!   evicted oldest-completed-first. This is the design's promise:
//!   the same retention window as the recent-calls pane, so the two
//!   never disagree about which calls are inspectable.
//! - **live** — a call exists for them ([`mark_live`], called when the
//!   control registry accepts the call). Capped at [`MAX_LIVE`] and
//!   evicted only when *that* is exceeded, never to make room for
//!   noise.
//! - **noise** — everything else: REGISTER refreshes, OPTIONS, and
//!   rejected scanner INVITEs. Capped at [`MAX_PENDING`], evicted
//!   least-recently-touched.
//!
//! Keying on "is it a call?" at capture time is not available: the
//! INVITE arrives *before* the call exists in any registry, and that
//! first message is the one most worth having — hence the two-step,
//! `push` first and `mark_live` when the call materialises.
//!
//! **Why live calls are a separate population** (fixed after the
//! 0.49.5 load run): an established call is SIP-silent between its ACK
//! and its BYE, so its `last_touched` never advances, while scanner
//! INVITEs keep arriving with fresh timestamps. Under one LRU pool the
//! bound therefore evicted **live calls first** — precisely inverting
//! what it was for, and discarding the ladder of the call an operator
//! is most likely to be looking at.

use std::collections::{HashMap, VecDeque};
use std::net::IpAddr;
use std::sync::{Mutex, OnceLock};
use std::time::UNIX_EPOCH;

use hep_rs::{HepPacket, HepProtocol, HepSink};
use siphon_ai_admin_api_types::SipMessageEntry;

/// Default completed-call retention; `[observability].sip_ring_size`
/// overrides. Matches `cdr_ring::DEFAULT_CAPACITY` by construction —
/// the two panes show the same window (design §2).
pub const DEFAULT_CAPACITY: usize = 50;

/// Default per-call message cap; `[observability].sip_ring_max_messages`
/// overrides. A normal call is 6–20 messages; 64 covers re-INVITE
/// churn, auth retries and a REFER without letting one pathological
/// dialog evict everyone else's history.
pub const DEFAULT_MAX_MESSAGES: usize = 64;

/// Non-call traces retained: REGISTER refreshes, OPTIONS, rejected
/// scanner INVITEs. Not configurable — a backstop, not a tuning knob;
/// an operator who wants less sets `sip_ring_size = 0`.
pub const MAX_PENDING: usize = 256;

/// Live-call traces retained. Separate from [`MAX_PENDING`] so noise
/// can never evict a call in progress. Sized well above any
/// concurrency this daemon has been load-tested at (203 in the 0.49.5
/// run), and it bounds memory: worst case is
/// `(MAX_PENDING + MAX_LIVE + cap_calls) × cap_messages` entries,
/// though a real call is 6–20 messages rather than the 64 cap.
///
/// In bytes: a retained message measures ~1.05 kB at a realistic
/// payload, so that ceiling is ~55 MB, while a prod-shaped node at
/// 200 concurrent pays ~1.9 MB — see
/// `test-harness/load/RESULTS-0.49.7-ring-ab.md`.
pub const MAX_LIVE: usize = 512;

#[derive(Clone)]
struct Trace {
    messages: VecDeque<SipMessageEntry>,
    truncated: bool,
    /// Monotonic touch counter, for least-recently-used eviction.
    /// Note this does **not** advance during an established call —
    /// see the module docs on why that made live calls evictable.
    last_touched: u64,
    completed: bool,
    /// A call exists for this dialog ([`mark_live`]).
    live: bool,
}

struct Inner {
    cap_calls: usize,
    cap_messages: usize,
    traces: HashMap<String, Trace>,
    /// Completed trace keys in completion order (oldest at the front).
    completed_order: VecDeque<String>,
    tick: u64,
}

fn ring() -> &'static Mutex<Inner> {
    static RING: OnceLock<Mutex<Inner>> = OnceLock::new();
    RING.get_or_init(|| {
        Mutex::new(Inner {
            cap_calls: DEFAULT_CAPACITY,
            cap_messages: DEFAULT_MAX_MESSAGES,
            traces: HashMap::new(),
            completed_order: VecDeque::new(),
            tick: 0,
        })
    })
}

/// Resize the ring (config load / reload). `cap_calls == 0` disables
/// capture entirely and drops everything held; shrinking evicts the
/// oldest completed traces.
pub fn set_capacity(cap_calls: usize, cap_messages: usize) {
    let mut inner = ring().lock().expect("sip ring poisoned");
    inner.cap_calls = cap_calls;
    inner.cap_messages = cap_messages.max(1);
    if cap_calls == 0 {
        inner.traces.clear();
        inner.completed_order.clear();
        return;
    }
    evict_completed(&mut inner);
    // Shrinking the per-call cap trims existing traces too, so the
    // bound is a live promise rather than one that only applies to
    // messages captured after the reload.
    let cap = inner.cap_messages;
    for trace in inner.traces.values_mut() {
        while trace.messages.len() > cap {
            trace.messages.pop_front();
            trace.truncated = true;
        }
    }
}

fn evict_completed(inner: &mut Inner) {
    while inner.completed_order.len() > inner.cap_calls {
        if let Some(key) = inner.completed_order.pop_front() {
            inner.traces.remove(&key);
        }
    }
}

/// Evict least-recently-touched traces from whichever population is
/// over its cap. Noise and live calls are bounded **separately**, so a
/// flood of scanner INVITEs can never displace a call in progress —
/// the inversion the 0.49.5 load run exposed. Returns how many were
/// dropped so the caller can bump a metric.
fn evict_pending(inner: &mut Inner) -> u64 {
    let mut dropped = 0;
    for (want_live, cap) in [(false, MAX_PENDING), (true, MAX_LIVE)] {
        loop {
            let n = inner
                .traces
                .values()
                .filter(|t| !t.completed && t.live == want_live)
                .count();
            if n <= cap {
                break;
            }
            let victim = inner
                .traces
                .iter()
                .filter(|(_, t)| !t.completed && t.live == want_live)
                .min_by_key(|(_, t)| t.last_touched)
                .map(|(k, _)| k.clone());
            match victim {
                Some(key) => {
                    inner.traces.remove(&key);
                    dropped += 1;
                }
                None => break,
            }
        }
    }
    dropped
}

/// Capture one SIP message against `sip_call_id`.
///
/// Called from the HEP sink leg — off the audio path by construction
/// (SIP is a handful of messages per call against 50 audio
/// frames/sec, CLAUDE.md §4.3), and a short mutex push with no I/O.
pub fn push(sip_call_id: &str, entry: SipMessageEntry) {
    let mut inner = ring().lock().expect("sip ring poisoned");
    if inner.cap_calls == 0 {
        return;
    }
    inner.tick += 1;
    let tick = inner.tick;
    let cap = inner.cap_messages;

    let trace = inner
        .traces
        .entry(sip_call_id.to_string())
        .or_insert_with(|| Trace {
            messages: VecDeque::new(),
            truncated: false,
            last_touched: tick,
            completed: false,
            live: false,
        });
    trace.last_touched = tick;
    if trace.messages.len() >= cap {
        trace.messages.pop_front();
        trace.truncated = true;
        metrics::counter!(crate::metrics::SIP_RING_MESSAGES_TOTAL, "result" => "dropped_call_cap")
            .increment(1);
    }
    trace.messages.push_back(entry);
    metrics::counter!(crate::metrics::SIP_RING_MESSAGES_TOTAL, "result" => "captured").increment(1);

    let dropped = evict_pending(&mut inner);
    if dropped > 0 {
        metrics::counter!(crate::metrics::SIP_RING_MESSAGES_TOTAL, "result" => "dropped_trace_cap")
            .increment(dropped);
    }
    publish_gauge(&inner);
}

/// Promote a trace to the live population — the control registry calls
/// this when a call is accepted for `sip_call_id`.
///
/// Idempotent, and a no-op for an id with no trace (capture disabled
/// when the INVITE arrived, or already evicted). The INVITE is always
/// captured *before* the call exists, which is why this is a second
/// step rather than a flag on [`push`].
pub fn mark_live(sip_call_id: &str) {
    let mut inner = ring().lock().expect("sip ring poisoned");
    if inner.cap_calls == 0 {
        return;
    }
    if let Some(t) = inner.traces.get_mut(sip_call_id) {
        t.live = true;
    }
    let dropped = evict_pending(&mut inner);
    if dropped > 0 {
        metrics::counter!(crate::metrics::SIP_RING_MESSAGES_TOTAL, "result" => "dropped_trace_cap")
            .increment(dropped);
    }
}

/// Mark a call complete — the runtime calls this from its ring CDR
/// sink, once per call end. Moves the trace into the completed
/// population, where it is retained until it falls out of the
/// `cap_calls` window.
///
/// A `sip_call_id` with no trace (capture disabled when the call
/// started, or the trace already evicted) is a no-op.
pub fn complete(sip_call_id: &str) {
    let mut inner = ring().lock().expect("sip ring poisoned");
    if inner.cap_calls == 0 {
        return;
    }
    match inner.traces.get_mut(sip_call_id) {
        Some(trace) if !trace.completed => {
            trace.completed = true;
            // Out of the live population and into the completed
            // window, which has its own cap.
            trace.live = false;
        }
        // Already completed (a duplicate CDR) or never captured.
        _ => return,
    }
    inner.completed_order.push_back(sip_call_id.to_string());
    evict_completed(&mut inner);
    publish_gauge(&inner);
}

fn publish_gauge(inner: &Inner) {
    metrics::gauge!(crate::metrics::SIP_RING_TRACES).set(inner.traces.len() as f64);
}

/// The messages held for `sip_call_id`, oldest first, plus whether
/// the per-call cap dropped anything. `None` when nothing is held.
pub fn snapshot(sip_call_id: &str) -> Option<(Vec<SipMessageEntry>, bool)> {
    let inner = ring().lock().expect("sip ring poisoned");
    inner
        .traces
        .get(sip_call_id)
        .map(|t| (t.messages.iter().cloned().collect(), t.truncated))
}

/// Serializes tests that mutate the process-global ring. The ring is
/// shared by `sip_ring`'s own tests and `admin`'s endpoint tests, and
/// cargo runs them on parallel threads — without this, one test's
/// `set_capacity` lands inside another's assertion. Crate-visible so
/// both modules take the *same* lock.
///
/// A `tokio::sync::Mutex` rather than a `std` one because `admin`'s
/// endpoint tests hold it across `dispatch(...).await`, which
/// `clippy::await_holding_lock` correctly rejects for a `std` guard.
#[cfg(test)]
pub(crate) fn test_mutex() -> &'static tokio::sync::Mutex<()> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(Default::default)
}

/// Blocking acquisition for the synchronous ring tests. Panics if
/// called from async context — the async tests use
/// `test_mutex().lock().await` instead.
#[cfg(test)]
pub(crate) fn test_lock() -> tokio::sync::MutexGuard<'static, ()> {
    test_mutex().blocking_lock()
}

/// `true` when capture is configured off, so the admin handler can
/// answer `501` rather than an empty-but-enabled-looking `200`.
pub fn is_enabled() -> bool {
    ring().lock().expect("sip ring poisoned").cap_calls > 0
}

/// The `HepSink` leg that feeds the ring. Non-SIP protocols
/// (RTCP/QoS, log, CDR, verstat) pass straight through untouched —
/// this leg only ever reads `HepProtocol::Sip`.
pub struct SipRingSink {
    /// Addresses that are "us", for deriving `direction`: the
    /// configured `[node].public_address` and the SIP bind IP when it
    /// is not a wildcard.
    ///
    /// Primarily matched on **IP**, never on port alone: SIP peers
    /// overwhelmingly send *from* 5060 as well, so a port-only test
    /// labels almost every inbound message "out". Port is consulted
    /// only to break a tie when both ends look local — see
    /// [`Self::direction`]. Unit tests lock both halves.
    local_ips: Vec<IpAddr>,
    /// The SIP listener's port, for the tie-break above.
    local_port: u16,
}

impl SipRingSink {
    pub fn new(local_ips: Vec<IpAddr>, local_port: u16) -> Self {
        Self {
            local_ips,
            local_port,
        }
    }

    /// Is this address one of ours?
    ///
    /// **An unspecified IP (`0.0.0.0` / `::`) counts as ours**, and
    /// that case is the common one rather than an edge: siphon-rs
    /// stamps a HEP packet's local end with the *socket's* address,
    /// which on the usual `listen = "0.0.0.0:5060"` is literally
    /// `0.0.0.0`. Without this, a wildcard-bound node — i.e. nearly
    /// every real deployment — matches neither end and renders every
    /// message `"unknown"`. Confirmed against a production node's own
    /// Homer capture: inbound `srcIp <peer> / dstIp 0.0.0.0`, outbound
    /// the reverse.
    fn is_local(&self, ip: IpAddr) -> bool {
        ip.is_unspecified() || self.local_ips.contains(&ip)
    }

    /// `"out"` when the message left us, `"in"` when it arrived, and
    /// `"unknown"` when neither end is recognisably this node.
    ///
    /// Three cases, in order:
    ///
    /// 1. **Exactly one end is ours** — that end decides. This is
    ///    every routed deployment.
    /// 2. **Both ends are ours** — a loopback lab, where src and dst
    ///    are both `127.0.0.1`. IP cannot discriminate, so the SIP
    ///    bind *port* does: a message leaving our listener is
    ///    outbound. This is the only place port is consulted, and
    ///    only after IP has already failed.
    /// 3. **Neither** — say so rather than guess. `src`/`dst` are in
    ///    every entry, so a client can always decide for itself.
    fn direction(&self, packet: &HepPacket) -> &'static str {
        let src_local = self.is_local(packet.src.ip());
        let dst_local = self.is_local(packet.dst.ip());
        match (src_local, dst_local) {
            (true, false) => "out",
            (false, true) => "in",
            (true, true) => {
                if packet.src.port() == self.local_port {
                    "out"
                } else if packet.dst.port() == self.local_port {
                    "in"
                } else {
                    "unknown"
                }
            }
            (false, false) => "unknown",
        }
    }
}

impl HepSink for SipRingSink {
    fn send(&self, packet: HepPacket) {
        if packet.protocol != HepProtocol::Sip {
            return;
        }
        let Some(call_id) = packet.correlation_id.as_deref() else {
            // No Call-ID to key on — nothing to attach it to.
            return;
        };
        let ts_ms = packet
            .timestamp
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        push(
            call_id,
            SipMessageEntry {
                ts_ms,
                direction: self.direction(&packet).to_string(),
                src: packet.src.to_string(),
                dst: packet.dst.to_string(),
                payload: String::from_utf8_lossy(&packet.payload).into_owned(),
            },
        );
    }
}

/// Fan-out over several `HepSink` legs, so SIP capture works whether
/// or not `[observability.hep]` ships to a collector (design §3.1).
/// Without this, teeing off the existing UDP sink would make the
/// ladder silently empty on a node with HEP disabled — the worst
/// failure mode for an observability feature, because it looks like
/// "no messages" rather than "not enabled".
pub struct FanOutHepSink {
    legs: Vec<std::sync::Arc<dyn HepSink>>,
}

impl FanOutHepSink {
    pub fn new(legs: Vec<std::sync::Arc<dyn HepSink>>) -> Self {
        Self { legs }
    }
}

impl HepSink for FanOutHepSink {
    fn send(&self, packet: HepPacket) {
        // Every leg is best-effort and non-blocking (CLAUDE.md §4.7);
        // the clone is one packet per SIP message, not per audio frame.
        for leg in &self.legs {
            leg.send(packet.clone());
        }
    }
}

fn _assert_send_sync() {
    fn is<T: Send + Sync>() {}
    is::<SipRingSink>();
    is::<FanOutHepSink>();
}

/// Milliseconds-since-epoch helper for callers building entries
/// outside the sink (tests).
#[cfg(test)]
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::SystemTime;

    // Process-global ring + parallel test threads — serialize on the
    // same lock `admin`'s endpoint tests take.
    use super::test_lock as serial;

    fn reset(cap_calls: usize, cap_messages: usize) {
        set_capacity(0, 1); // clears
        set_capacity(cap_calls, cap_messages);
    }

    fn entry(payload: &str) -> SipMessageEntry {
        SipMessageEntry {
            ts_ms: now_ms(),
            direction: "in".into(),
            src: "10.0.0.1:5060".into(),
            dst: "10.0.0.2:5060".into(),
            payload: payload.into(),
        }
    }

    #[test]
    fn per_call_cap_drops_oldest_within_the_call_and_flags_truncation() {
        let _serial = serial();
        reset(10, 3);
        for i in 0..5 {
            push("call-a", entry(&format!("MSG{i}")));
        }
        let (msgs, truncated) = snapshot("call-a").expect("trace held");
        assert_eq!(msgs.len(), 3, "per-call cap holds");
        assert_eq!(msgs[0].payload, "MSG2", "oldest within the call went");
        assert_eq!(
            msgs[2].payload, "MSG4",
            "newest retained, oldest-first order"
        );
        assert!(truncated, "truncation is reported, not silent");
    }

    #[test]
    fn completed_window_evicts_whole_calls_in_completion_order() {
        let _serial = serial();
        reset(2, 8);
        for i in 0..3 {
            let id = format!("call-{i}");
            push(&id, entry("INVITE"));
            complete(&id);
        }
        assert!(snapshot("call-0").is_none(), "oldest completed evicted");
        assert!(snapshot("call-1").is_some());
        assert!(snapshot("call-2").is_some());
    }

    #[test]
    fn live_calls_are_never_evicted_by_the_completed_window() {
        let _serial = serial();
        reset(1, 8);
        push("live", entry("INVITE"));
        for i in 0..5 {
            let id = format!("done-{i}");
            push(&id, entry("INVITE"));
            complete(&id);
        }
        assert!(
            snapshot("live").is_some(),
            "an in-progress call survives a full completed window"
        );
        assert!(snapshot("done-4").is_some(), "newest completed retained");
        assert!(snapshot("done-0").is_none());
    }

    // The case the design note missed: REGISTER refreshes, OPTIONS and
    // scanner INVITEs carry Call-IDs, reach this sink, and never
    // complete. Without a pending bound they accumulate forever.
    #[test]
    fn pending_population_is_bounded_for_traffic_that_never_completes() {
        let _serial = serial();
        reset(50, 4);
        for i in 0..(MAX_PENDING + 40) {
            push(&format!("scanner-{i}"), entry("INVITE"));
        }
        let held = ring().lock().unwrap().traces.len();
        assert!(
            held <= MAX_PENDING,
            "pending traces bounded at {MAX_PENDING}, held {held}"
        );
        assert!(
            snapshot("scanner-0").is_none(),
            "least-recently-touched pending trace evicted first"
        );
        let newest = format!("scanner-{}", MAX_PENDING + 39);
        assert!(snapshot(&newest).is_some(), "most recent pending retained");
    }

    // The 0.49.5 load run's finding, as a test. An established call is
    // SIP-silent between ACK and BYE, so its last_touched never
    // advances while scanner INVITEs keep arriving with fresh ones.
    // Under a single LRU pool the live call was the *first* thing
    // evicted — the exact inversion of what the bound is for.
    #[test]
    fn a_flood_of_noise_cannot_evict_a_live_call() {
        let _serial = serial();
        reset(50, 8);

        // A call arrives and is accepted, then goes quiet.
        push("the-live-call", entry("INVITE"));
        push("the-live-call", entry("ACK"));
        mark_live("the-live-call");

        // Then far more non-call traffic than the noise cap, all of it
        // touched more recently than the silent call.
        for i in 0..(MAX_PENDING + 100) {
            push(&format!("scanner-{i}"), entry("INVITE"));
        }

        let held = snapshot("the-live-call");
        assert!(
            held.is_some(),
            "a live call must survive any amount of noise"
        );
        assert_eq!(held.unwrap().0.len(), 2, "and keep its messages");

        // The noise is still bounded — the fix must not trade one
        // unbounded population for another.
        let noise = ring()
            .lock()
            .unwrap()
            .traces
            .values()
            .filter(|t| !t.completed && !t.live)
            .count();
        assert!(noise <= MAX_PENDING, "noise still capped, held {noise}");
    }

    #[test]
    fn live_calls_are_bounded_too_and_evict_oldest_first() {
        let _serial = serial();
        reset(50, 8);
        for i in 0..(MAX_LIVE + 20) {
            let id = format!("call-{i}");
            push(&id, entry("INVITE"));
            mark_live(&id);
        }
        let live = ring()
            .lock()
            .unwrap()
            .traces
            .values()
            .filter(|t| t.live)
            .count();
        assert!(live <= MAX_LIVE, "live population bounded, held {live}");
        assert!(snapshot("call-0").is_none(), "oldest live evicted first");
        let newest = format!("call-{}", MAX_LIVE + 19);
        assert!(snapshot(&newest).is_some(), "newest live retained");
    }

    // Completing a call takes it out of the live population, so a
    // long-running node cannot accumulate "live" traces for calls that
    // ended.
    #[test]
    fn completing_a_call_releases_its_live_slot() {
        let _serial = serial();
        reset(50, 8);
        push("c1", entry("INVITE"));
        mark_live("c1");
        assert!(ring().lock().unwrap().traces["c1"].live);
        complete("c1");
        let t = &ring().lock().unwrap().traces["c1"];
        assert!(!t.live && t.completed, "moved live → completed");
    }

    #[test]
    fn mark_live_is_idempotent_and_ignores_unknown_ids() {
        let _serial = serial();
        reset(50, 8);
        mark_live("never-seen"); // must not panic or create
        assert!(snapshot("never-seen").is_none());
        push("c2", entry("INVITE"));
        mark_live("c2");
        mark_live("c2");
        assert!(snapshot("c2").is_some());
    }

    #[test]
    fn zero_disables_and_drops_everything_held() {
        let _serial = serial();
        reset(10, 8);
        push("call-a", entry("INVITE"));
        assert!(snapshot("call-a").is_some());
        assert!(is_enabled());

        set_capacity(0, 8);
        assert!(!is_enabled(), "501 path sees capture as off");
        assert!(snapshot("call-a").is_none(), "held messages dropped");
        push("call-b", entry("INVITE"));
        assert!(snapshot("call-b").is_none(), "capture stops");
        reset(DEFAULT_CAPACITY, DEFAULT_MAX_MESSAGES);
    }

    #[test]
    fn shrinking_the_message_cap_trims_traces_already_held() {
        let _serial = serial();
        reset(10, 8);
        for i in 0..8 {
            push("call-a", entry(&format!("MSG{i}")));
        }
        assert_eq!(snapshot("call-a").unwrap().0.len(), 8);
        set_capacity(10, 2);
        let (msgs, truncated) = snapshot("call-a").unwrap();
        assert_eq!(msgs.len(), 2, "reload trims what is already held");
        assert_eq!(msgs[1].payload, "MSG7");
        assert!(truncated);
    }

    #[test]
    fn sink_ignores_non_sip_protocols_and_packets_without_a_call_id() {
        let _serial = serial();
        reset(10, 8);
        let sink = SipRingSink::new(vec!["10.0.0.2".parse().unwrap()], 5060);

        let base = HepPacket {
            capture_id: 1,
            capture_password: None,
            protocol: HepProtocol::Rtcp,
            transport: hep_rs::IpProto::Udp,
            src: "10.0.0.1:4000".parse().unwrap(),
            dst: "10.0.0.2:4000".parse().unwrap(),
            timestamp: SystemTime::now(),
            correlation_id: Some("call-x".into()),
            payload: b"rtcp".to_vec(),
        };
        sink.send(base.clone());
        assert!(snapshot("call-x").is_none(), "RTCP is not signaling");

        let no_id = HepPacket {
            protocol: HepProtocol::Sip,
            correlation_id: None,
            ..base.clone()
        };
        sink.send(no_id);

        let sip = HepPacket {
            protocol: HepProtocol::Sip,
            src: "203.0.113.9:5060".parse().unwrap(),
            dst: "10.0.0.2:5060".parse().unwrap(),
            payload: b"INVITE sip:x SIP/2.0\r\n".to_vec(),
            ..base
        };
        sink.send(sip);
        let (msgs, _) = snapshot("call-x").expect("SIP captured");
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].direction, "in", "inbound: src is not our bind");
        assert!(msgs[0].payload.starts_with("INVITE"));
    }

    #[test]
    fn direction_is_out_when_the_source_is_our_own_bind() {
        let _serial = serial();
        reset(10, 8);
        let sink = SipRingSink::new(vec!["139.177.205.140".parse().unwrap()], 5060);
        sink.send(HepPacket {
            capture_id: 1,
            capture_password: None,
            protocol: HepProtocol::Sip,
            transport: hep_rs::IpProto::Udp,
            src: "139.177.205.140:5060".parse().unwrap(),
            dst: "194.195.208.34:5060".parse().unwrap(),
            timestamp: SystemTime::now(),
            correlation_id: Some("call-out".into()),
            payload: b"REGISTER sip:x SIP/2.0\r\n".to_vec(),
        });
        let (msgs, _) = snapshot("call-out").unwrap();
        assert_eq!(msgs[0].direction, "out");
    }

    // The bug a port-based derivation shipped with: a peer sending
    // *from* 5060 to our 5060 bind is the overwhelmingly common
    // inbound case, and matching on port labelled all of it "out".
    #[test]
    fn inbound_from_a_peer_on_port_5060_is_not_labelled_outbound() {
        let _serial = serial();
        reset(10, 8);
        let sink = SipRingSink::new(vec!["139.177.205.140".parse().unwrap()], 5060);
        sink.send(HepPacket {
            capture_id: 1,
            capture_password: None,
            protocol: HepProtocol::Sip,
            transport: hep_rs::IpProto::Udp,
            // Same port as our bind, different host: inbound.
            src: "203.0.113.9:5060".parse().unwrap(),
            dst: "139.177.205.140:5060".parse().unwrap(),
            timestamp: SystemTime::now(),
            correlation_id: Some("peer-5060".into()),
            payload: b"INVITE sip:x SIP/2.0\r\n".to_vec(),
        });
        assert_eq!(snapshot("peer-5060").unwrap().0[0].direction, "in");
    }

    // Found by running the 0.49.3 artifact against a production-shaped
    // node. siphon-rs stamps a HEP packet's local end with the
    // *socket's* address, so on `listen = "0.0.0.0:5060"` — nearly
    // every real deployment — our own end is literally 0.0.0.0.
    // Matching only the configured public address meant neither end
    // matched and every message rendered "unknown". Taken verbatim
    // from prod's Homer capture.
    #[test]
    fn wildcard_bind_stamps_our_end_unspecified_and_still_resolves() {
        let _serial = serial();
        reset(10, 8);
        let sink = SipRingSink::new(vec!["139.177.205.140".parse().unwrap()], 5060);
        let base = HepPacket {
            capture_id: 1,
            capture_password: None,
            protocol: HepProtocol::Sip,
            transport: hep_rs::IpProto::Udp,
            src: "194.195.208.34:5060".parse().unwrap(),
            dst: "0.0.0.0:5060".parse().unwrap(),
            timestamp: SystemTime::now(),
            correlation_id: Some("wildcard-in".into()),
            payload: b"SIP/2.0 401 Unauthorized\r\n".to_vec(),
        };
        sink.send(base.clone());
        assert_eq!(
            snapshot("wildcard-in").unwrap().0[0].direction,
            "in",
            "arrived at our wildcard-bound listener"
        );

        sink.send(HepPacket {
            src: "0.0.0.0:5060".parse().unwrap(),
            dst: "194.195.208.34:5060".parse().unwrap(),
            correlation_id: Some("wildcard-out".into()),
            ..base
        });
        assert_eq!(
            snapshot("wildcard-out").unwrap().0[0].direction,
            "out",
            "left our wildcard-bound listener"
        );
    }

    // The other half of the same bug: on loopback both ends are ours,
    // so IP cannot discriminate and everything read "out". The SIP
    // bind port breaks the tie — and only here, after IP has failed,
    // because a port-first test mislabels inbound traffic (peers send
    // *from* 5060 too, locked by the test above).
    #[test]
    fn loopback_uses_the_bind_port_to_break_the_tie() {
        let _serial = serial();
        reset(10, 8);
        let sink = SipRingSink::new(vec!["127.0.0.1".parse().unwrap()], 5070);
        let base = HepPacket {
            capture_id: 1,
            capture_password: None,
            protocol: HepProtocol::Sip,
            transport: hep_rs::IpProto::Udp,
            src: "127.0.0.1:52620".parse().unwrap(),
            dst: "127.0.0.1:5070".parse().unwrap(),
            timestamp: SystemTime::now(),
            correlation_id: Some("lo-in".into()),
            payload: b"INVITE sip:x SIP/2.0\r\n".to_vec(),
        };
        sink.send(base.clone());
        assert_eq!(
            snapshot("lo-in").unwrap().0[0].direction,
            "in",
            "an INVITE arriving at our listener is inbound, even on loopback"
        );

        sink.send(HepPacket {
            src: "127.0.0.1:5070".parse().unwrap(),
            dst: "127.0.0.1:52620".parse().unwrap(),
            correlation_id: Some("lo-out".into()),
            ..base
        });
        assert_eq!(snapshot("lo-out").unwrap().0[0].direction, "out");
    }

    #[test]
    fn direction_is_unknown_when_neither_end_is_recognisably_us() {
        let _serial = serial();
        reset(10, 8);
        // Wildcard bind, no public_address configured.
        let sink = SipRingSink::new(vec![], 5060);
        sink.send(HepPacket {
            capture_id: 1,
            capture_password: None,
            protocol: HepProtocol::Sip,
            transport: hep_rs::IpProto::Udp,
            src: "203.0.113.9:5060".parse().unwrap(),
            dst: "10.0.0.2:5060".parse().unwrap(),
            timestamp: SystemTime::now(),
            correlation_id: Some("no-idea".into()),
            payload: b"OPTIONS sip:x SIP/2.0\r\n".to_vec(),
        });
        assert_eq!(
            snapshot("no-idea").unwrap().0[0].direction,
            "unknown",
            "saying so beats guessing; src/dst are in the entry"
        );
    }

    #[test]
    fn fan_out_delivers_to_every_leg() {
        let _serial = serial();
        reset(10, 8);
        #[derive(Default)]
        struct Count(Mutex<usize>);
        impl HepSink for Count {
            fn send(&self, _p: HepPacket) {
                *self.0.lock().unwrap() += 1;
            }
        }
        let a = std::sync::Arc::new(Count::default());
        let b = std::sync::Arc::new(Count::default());
        let fan = FanOutHepSink::new(vec![a.clone(), b.clone()]);
        fan.send(HepPacket {
            capture_id: 1,
            capture_password: None,
            protocol: HepProtocol::Sip,
            transport: hep_rs::IpProto::Udp,
            src: "10.0.0.1:5060".parse().unwrap(),
            dst: "10.0.0.2:5060".parse().unwrap(),
            timestamp: SystemTime::now(),
            correlation_id: Some("c".into()),
            payload: b"x".to_vec(),
        });
        assert_eq!(*a.0.lock().unwrap(), 1);
        assert_eq!(*b.0.lock().unwrap(), 1);
    }
}
