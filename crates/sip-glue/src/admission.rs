//! Inbound INVITE admission control (per-source rate limiting + a
//! global concurrency cap).
//!
//! Runs as the **first** gate on a new out-of-dialog INVITE — before the
//! drain check, trunk allowlist, digest auth, and routing — so an abusive
//! source is shed as cheaply as possible. Two independent limits:
//!
//! - **Per-source token bucket** keyed on the source IP (`max_per_sec` +
//!   `burst`). A source that exceeds its rate is answered `503` +
//!   `Retry-After`; after `drop_after` consecutive rejects it is
//!   **silently dropped** (an obvious flood doesn't earn a response).
//! - **Global concurrency cap** (`max_concurrent`): a new INVITE is
//!   answered `503` when the live call count is already at the cap. The
//!   count is read through a closure the daemon supplies over its
//!   `CallRegistry`, so this crate stays free of a core dependency.
//!
//! Source buckets live in a size-capped table (`max_sources`) with
//! idle/oldest eviction, so the limiter itself can't leak memory under a
//! spoofed-source flood.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

/// Reads the current number of live calls (for the global cap). Supplied
/// by the daemon over `CallRegistry::len()`.
pub type ActiveCallCountFn = Arc<dyn Fn() -> usize + Send + Sync>;

/// Idle sources are evicted after this long without an INVITE.
const IDLE_EVICT: Duration = Duration::from_secs(300);
/// `Retry-After` (seconds) advertised on an admission `503`.
pub const ADMISSION_RETRY_AFTER_SECS: u32 = 2;

/// What to do with a new INVITE per the admission policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdmissionDecision {
    /// Admit — continue to the drain/trunk/auth/route gates.
    Accept,
    /// Reject with `503 Service Unavailable` + `Retry-After` (a normal
    /// rate trip or the global cap).
    Reject503,
    /// Silently drop — no response. The source is flooding past
    /// `drop_after`; spending a `503` per packet would only amplify.
    Drop,
}

impl AdmissionDecision {
    /// The `siphon_ai_invite_admission_total{result}` label.
    pub fn metric_result(&self) -> &'static str {
        match self {
            AdmissionDecision::Accept => "accepted",
            AdmissionDecision::Reject503 => "rate_limited",
            AdmissionDecision::Drop => "dropped",
        }
    }
}

struct Bucket {
    tokens: f64,
    last: Instant,
    /// Consecutive rejects since the last admit — drives the `503` →
    /// silent-drop escalation.
    consecutive_rejects: u32,
    /// Last touch, for idle/oldest eviction.
    last_seen: Instant,
}

/// Inbound INVITE admission limiter.
pub struct InviteAdmission {
    /// Per-source buckets. `None` when `max_per_sec == 0` (global cap
    /// only).
    sources: Option<Mutex<HashMap<IpAddr, Bucket>>>,
    rate: f64,
    capacity: f64,
    drop_after: u32,
    max_sources: usize,
    /// Global concurrent-call cap; `0` ⇒ no cap.
    max_concurrent: usize,
    active_count: ActiveCallCountFn,
}

impl InviteAdmission {
    pub fn new(
        max_per_sec: u32,
        burst: u32,
        drop_after: u32,
        max_concurrent: u32,
        max_sources: u32,
        active_count: ActiveCallCountFn,
    ) -> Self {
        let per_source = max_per_sec > 0;
        Self {
            sources: per_source.then(|| Mutex::new(HashMap::new())),
            rate: max_per_sec as f64,
            capacity: burst.max(max_per_sec) as f64,
            drop_after,
            max_sources: max_sources as usize,
            max_concurrent: max_concurrent as usize,
            active_count,
        }
    }

    /// Decide whether to admit a new INVITE from `src`.
    pub fn check(&self, src: IpAddr) -> AdmissionDecision {
        self.check_at(src, Instant::now())
    }

    fn check_at(&self, src: IpAddr, now: Instant) -> AdmissionDecision {
        // 1. Global concurrency cap first — cheapest, and an overloaded
        //    node should shed regardless of source.
        if self.max_concurrent > 0 && (self.active_count)() >= self.max_concurrent {
            return AdmissionDecision::Reject503;
        }
        // 2. Per-source token bucket.
        let Some(sources) = self.sources.as_ref() else {
            return AdmissionDecision::Accept;
        };
        let mut table = sources.lock();
        self.evict_if_full(&mut table, now);
        let bucket = table.entry(src).or_insert_with(|| Bucket {
            tokens: self.capacity,
            last: now,
            consecutive_rejects: 0,
            last_seen: now,
        });
        bucket.last_seen = now;
        let elapsed = now.saturating_duration_since(bucket.last).as_secs_f64();
        bucket.last = now;
        bucket.tokens = (bucket.tokens + elapsed * self.rate).min(self.capacity);
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            bucket.consecutive_rejects = 0;
            AdmissionDecision::Accept
        } else {
            bucket.consecutive_rejects = bucket.consecutive_rejects.saturating_add(1);
            if bucket.consecutive_rejects > self.drop_after {
                AdmissionDecision::Drop
            } else {
                AdmissionDecision::Reject503
            }
        }
    }

    /// Bound the table: when at/over capacity, drop idle entries first,
    /// then the oldest, until under `max_sources`.
    fn evict_if_full(&self, table: &mut HashMap<IpAddr, Bucket>, now: Instant) {
        if table.len() < self.max_sources {
            return;
        }
        table.retain(|_, b| now.saturating_duration_since(b.last_seen) < IDLE_EVICT);
        while table.len() >= self.max_sources {
            // Evict the least-recently-seen entry.
            if let Some(oldest) = table
                .iter()
                .min_by_key(|(_, b)| b.last_seen)
                .map(|(ip, _)| *ip)
            {
                table.remove(&oldest);
            } else {
                break;
            }
        }
    }

    /// Number of source IPs currently tracked (for the gauge).
    pub fn source_count(&self) -> usize {
        self.sources.as_ref().map(|s| s.lock().len()).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn ip(n: u8) -> IpAddr {
        IpAddr::from([10, 0, 0, n])
    }

    fn zero_count() -> ActiveCallCountFn {
        Arc::new(|| 0)
    }

    #[test]
    fn per_source_bucket_limits_then_drops() {
        // rate 2/s, burst 2, drop_after 2.
        let a = InviteAdmission::new(2, 2, 2, 0, 1000, zero_count());
        let t0 = Instant::now();
        // Two burst tokens admit.
        assert_eq!(a.check_at(ip(1), t0), AdmissionDecision::Accept);
        assert_eq!(a.check_at(ip(1), t0), AdmissionDecision::Accept);
        // Next two are 503 (rejects 1 and 2, both ≤ drop_after).
        assert_eq!(a.check_at(ip(1), t0), AdmissionDecision::Reject503);
        assert_eq!(a.check_at(ip(1), t0), AdmissionDecision::Reject503);
        // Third reject (> drop_after = 2) escalates to silent drop.
        assert_eq!(a.check_at(ip(1), t0), AdmissionDecision::Drop);
    }

    #[test]
    fn bucket_refills_over_time() {
        let a = InviteAdmission::new(2, 2, 5, 0, 1000, zero_count());
        let t0 = Instant::now();
        assert_eq!(a.check_at(ip(1), t0), AdmissionDecision::Accept);
        assert_eq!(a.check_at(ip(1), t0), AdmissionDecision::Accept);
        assert_eq!(a.check_at(ip(1), t0), AdmissionDecision::Reject503);
        // One second later ~2 tokens refilled.
        let t1 = t0 + Duration::from_secs(1);
        assert_eq!(a.check_at(ip(1), t1), AdmissionDecision::Accept);
    }

    #[test]
    fn sources_are_independent() {
        let a = InviteAdmission::new(1, 1, 5, 0, 1000, zero_count());
        let t0 = Instant::now();
        assert_eq!(a.check_at(ip(1), t0), AdmissionDecision::Accept);
        assert_eq!(a.check_at(ip(1), t0), AdmissionDecision::Reject503);
        // A different source has its own full bucket.
        assert_eq!(a.check_at(ip(2), t0), AdmissionDecision::Accept);
    }

    #[test]
    fn global_cap_rejects_when_at_limit() {
        let active = Arc::new(AtomicUsize::new(0));
        let a = {
            let active = Arc::clone(&active);
            InviteAdmission::new(
                0,
                0,
                5,
                3,
                1000,
                Arc::new(move || active.load(Ordering::Relaxed)),
            )
        };
        // No per-source rate; under the cap → accept.
        active.store(2, Ordering::Relaxed);
        assert_eq!(a.check(ip(1)), AdmissionDecision::Accept);
        // At the cap → 503.
        active.store(3, Ordering::Relaxed);
        assert_eq!(a.check(ip(1)), AdmissionDecision::Reject503);
    }

    #[test]
    fn table_is_bounded() {
        let a = InviteAdmission::new(1, 1, 5, 0, 4, zero_count());
        let t0 = Instant::now();
        for n in 0..20u8 {
            a.check_at(ip(n), t0);
        }
        assert!(
            a.source_count() <= 4,
            "table should stay bounded at max_sources, got {}",
            a.source_count()
        );
    }

    #[test]
    fn disabled_per_source_accepts() {
        // Only a global cap; per-source table is absent.
        let a = InviteAdmission::new(0, 0, 5, 0, 1000, zero_count());
        for _ in 0..100 {
            assert_eq!(a.check(ip(1)), AdmissionDecision::Accept);
        }
        assert_eq!(a.source_count(), 0);
    }
}
