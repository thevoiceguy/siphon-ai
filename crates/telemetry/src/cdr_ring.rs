//! Recent-CDRs ring (0.49.0, DESIGN_SIGHTGLASS.md §6.3).
//!
//! The last N completed-call CDRs, kept in memory and served
//! newest-first by `GET /admin/v1/cdrs/recent` — the "what just
//! happened" tail behind sightglass's call-history pane, and a
//! curl-able incident tool when no CDR sink is configured.
//!
//! Entries are the **serialized `CdrRecord`** (`serde_json::Value`) —
//! the CDR schema is the one schema (design §6.3: "no second
//! schema"); this crate deliberately doesn't depend on
//! `siphon-ai-cdr`, so the runtime's ring sink serializes and pushes.
//! Process-global for the same reason `error_ring` is: the capturing
//! sink is rebuilt on SIGHUP reloads while the ring — and its
//! configured capacity — survives.

use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};

/// Default ring capacity; `[observability].cdr_ring_size` overrides.
pub const DEFAULT_CAPACITY: usize = 50;

struct Inner {
    cap: usize,
    buf: VecDeque<serde_json::Value>,
}

fn ring() -> &'static Mutex<Inner> {
    static RING: OnceLock<Mutex<Inner>> = OnceLock::new();
    RING.get_or_init(|| {
        Mutex::new(Inner {
            cap: DEFAULT_CAPACITY,
            buf: VecDeque::with_capacity(DEFAULT_CAPACITY),
        })
    })
}

/// Resize the ring (config load / reload). `0` disables capture;
/// shrinking drops the oldest entries.
pub fn set_capacity(cap: usize) {
    let mut inner = ring().lock().expect("cdr ring poisoned");
    inner.cap = cap;
    while inner.buf.len() > cap {
        inner.buf.pop_front();
    }
}

/// Push one completed call's serialized CDR. Called by the runtime's
/// ring sink at CDR emission — once per call end, never on the audio
/// path.
pub fn push(record: serde_json::Value) {
    let mut inner = ring().lock().expect("cdr ring poisoned");
    if inner.cap == 0 {
        return;
    }
    if inner.buf.len() == inner.cap {
        inner.buf.pop_front();
    }
    inner.buf.push_back(record);
}

/// Snapshot the ring, newest first.
pub fn snapshot() -> Vec<serde_json::Value> {
    let inner = ring().lock().expect("cdr ring poisoned");
    inner.buf.iter().rev().cloned().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // Process-global ring + parallel test threads — serialize.
    fn serial() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn bounded_newest_first_and_zero_disables() {
        let _serial = serial();
        set_capacity(3);
        for i in 0..5 {
            push(json!({ "call_id": format!("c{i}") }));
        }
        let snap = snapshot();
        assert_eq!(snap.len(), 3);
        assert_eq!(snap[0]["call_id"], "c4");
        assert_eq!(snap[2]["call_id"], "c2");

        set_capacity(0);
        push(json!({ "call_id": "dropped" }));
        assert!(snapshot().is_empty());
        set_capacity(DEFAULT_CAPACITY);
    }
}
