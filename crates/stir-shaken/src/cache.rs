//! Process-wide TTL cache for fetched signing-certificate chains.
//!
//! `x5u` certificate fetches are slow (an HTTPS round-trip on the call
//! accept path) and highly repetitive — a busy trunk re-sends the same
//! signer's certificate on every call. Caching the parsed DER chain keyed
//! by `x5u` URL turns all but the first verification of a given signer into
//! a lock-and-clone (plan §9 decision 2: TTL eviction, default 1h).
//!
//! This is shared across calls on purpose — it is process-wide
//! infrastructure (like metrics), not per-call state, so it does not
//! violate CLAUDE.md §4.4. The `std::sync::Mutex` is only ever held for the
//! map lookup/insert and never across an `await`, and the cache lives on
//! the signaling (accept) path, never the audio hot path — so §4.3 does not
//! apply.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// A cached certificate chain. `Arc` so a cache hit clones a pointer, not
/// the (potentially several-KB) DER bytes.
type Chain = Arc<Vec<Vec<u8>>>;

struct Entry {
    chain: Chain,
    expires_at: Instant,
}

/// TTL cache of `x5u` URL → certificate chain.
#[derive(Default)]
pub(crate) struct CertCache {
    entries: Mutex<HashMap<String, Entry>>,
}

impl CertCache {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Return the cached chain for `url` if present and not expired at
    /// `now`. An expired entry is evicted on access so a stale signer's
    /// bytes don't linger.
    pub(crate) fn get(&self, url: &str, now: Instant) -> Option<Chain> {
        let mut entries = self.lock();
        match entries.get(url) {
            Some(entry) if entry.expires_at > now => Some(entry.chain.clone()),
            Some(_) => {
                entries.remove(url);
                None
            }
            None => None,
        }
    }

    /// Insert (or replace) the chain for `url`, expiring at `expires_at`.
    pub(crate) fn insert(&self, url: String, chain: Chain, expires_at: Instant) {
        self.lock().insert(url, Entry { chain, expires_at });
    }

    /// Lock the map, recovering from poisoning rather than propagating a
    /// panic — a cert cache must not be able to wedge verification for the
    /// life of the process if some unrelated holder panicked.
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Entry>> {
        self.entries.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn chain(tag: u8) -> Chain {
        Arc::new(vec![vec![tag; 4]])
    }

    #[test]
    fn hit_before_expiry_miss_after() {
        let cache = CertCache::new();
        let t0 = Instant::now();
        cache.insert(
            "https://c/a.crt".into(),
            chain(1),
            t0 + Duration::from_secs(60),
        );

        // Hit while fresh.
        let got = cache
            .get("https://c/a.crt", t0 + Duration::from_secs(30))
            .unwrap();
        assert_eq!(*got, vec![vec![1u8; 4]]);

        // Miss once past the expiry instant.
        assert!(cache
            .get("https://c/a.crt", t0 + Duration::from_secs(61))
            .is_none());
    }

    #[test]
    fn unknown_url_misses() {
        let cache = CertCache::new();
        assert!(cache.get("https://c/none.crt", Instant::now()).is_none());
    }

    #[test]
    fn insert_replaces_existing() {
        let cache = CertCache::new();
        let t0 = Instant::now();
        let exp = t0 + Duration::from_secs(60);
        cache.insert("https://c/a.crt".into(), chain(1), exp);
        cache.insert("https://c/a.crt".into(), chain(2), exp);
        let got = cache.get("https://c/a.crt", t0).unwrap();
        assert_eq!(*got, vec![vec![2u8; 4]]);
    }

    #[test]
    fn expired_entry_is_evicted_on_access() {
        let cache = CertCache::new();
        let t0 = Instant::now();
        cache.insert(
            "https://c/a.crt".into(),
            chain(1),
            t0 + Duration::from_secs(10),
        );
        // Access past expiry returns None and removes the entry.
        assert!(cache
            .get("https://c/a.crt", t0 + Duration::from_secs(11))
            .is_none());
        // Confirm eviction: even querying back before the old expiry misses.
        assert!(cache.get("https://c/a.crt", t0).is_none());
    }
}
