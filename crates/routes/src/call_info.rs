//! Inputs the matcher evaluates a route against.
//!
//! `CallInfo` is everything the matcher might inspect for one
//! incoming INVITE: SIP URIs, registration source, custom headers.
//! It's intentionally borrowed (`&str`) — the matcher walks it once
//! per call setup and never mutates anything.
//!
//! ## `register_source` semantics
//!
//! - `"trunk"` for inbound calls that arrived on the SIP listener
//!   without traversing one of our `[[register]]` blocks (i.e.,
//!   trunk mode);
//! - the `name` field of the matching `[[register]]` block when the
//!   call arrived through a registered AOR.
//!
//! The sip-glue crate is responsible for setting this correctly.
//!
//! ## Header lookup
//!
//! Header names are case-insensitive. The caller (sip-glue) builds a
//! `Headers` map by lowercasing each name; the matcher only ever
//! looks up lowercase keys.

use std::collections::HashMap;

/// Lowercased-key map of inbound headers. Use [`Headers::insert`] to
/// canonicalize names automatically.
#[derive(Debug, Clone, Default)]
pub struct Headers {
    inner: HashMap<String, String>,
}

impl Headers {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a header. The name is lowercased; the value is stored
    /// verbatim. If the same name is inserted twice, the last write
    /// wins — sip-glue is responsible for picking the right
    /// representative when SIP allows multiple instances of a header.
    pub fn insert(&mut self, name: &str, value: impl Into<String>) {
        self.inner.insert(name.to_ascii_lowercase(), value.into());
    }

    /// Look up `name` case-insensitively. Returns the stored value
    /// or `None` if absent.
    pub fn get(&self, name: &str) -> Option<&str> {
        // The map is keyed lowercase, so allocate a lowercase copy
        // only if `name` isn't already lowercase. Most callers pass
        // already-lowercased names from the route-config side.
        if name.bytes().all(|b| !b.is_ascii_uppercase()) {
            self.inner.get(name).map(String::as_str)
        } else {
            self.inner
                .get(&name.to_ascii_lowercase())
                .map(String::as_str)
        }
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    pub fn len(&self) -> usize {
        self.inner.len()
    }
}

/// Everything a route's `[match]` block can predicate on.
///
/// All fields are required — the caller substitutes the empty string
/// for SIP fields that are genuinely absent (a malformed INVITE that
/// passes parsing but lacks, say, a `To`-username). An empty
/// candidate field never matches a non-empty literal or regex
/// pattern; it does match `any = true`.
#[derive(Debug, Clone)]
pub struct CallInfo<'a> {
    pub request_uri_user: &'a str,
    pub request_uri_host: &'a str,

    pub to_user: &'a str,
    pub to_host: &'a str,

    pub from_user: &'a str,
    pub from_host: &'a str,

    /// `"trunk"` for unregistered inbound; otherwise the matching
    /// `[[register]]` block's `name`.
    pub register_source: &'a str,

    pub headers: &'a Headers,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headers_lookup_is_case_insensitive() {
        let mut h = Headers::new();
        h.insert("X-Customer-Id", "cust-42");
        assert_eq!(h.get("x-customer-id"), Some("cust-42"));
        assert_eq!(h.get("X-CUSTOMER-ID"), Some("cust-42"));
        assert_eq!(h.get("X-Customer-Id"), Some("cust-42"));
        assert_eq!(h.get("x-other"), None);
    }

    #[test]
    fn last_write_wins() {
        let mut h = Headers::new();
        h.insert("X-K", "v1");
        h.insert("x-k", "v2");
        assert_eq!(h.get("X-K"), Some("v2"));
        assert_eq!(h.len(), 1);
    }
}
