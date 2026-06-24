//! Walks `RawRouteFile` → `RouteSet`.
//!
//! Validation rules per `docs/DEV_PLAN.md` §6.5 and CLAUDE.md §4.6:
//!
//! - Every route has a non-empty `name`.
//! - Every match block has at least one positive predicate (a string
//!   field, a header, or `any = true`).
//! - `any = true` is mutually exclusive with all other match keys —
//!   silently allowing other keys would be a footgun: which fact
//!   "won"?
//! - When a route sets `regex = true`, every string match value in
//!   that route's match block must compile as a Rust regex.
//! - `any = true` on a non-trailing route is allowed (it just means
//!   later routes are unreachable). We surface a soft warning at
//!   the daemon level rather than failing here, because legitimate
//!   reload-test workflows ("temporarily route everything to X")
//!   want this.

use thiserror::Error;

use crate::compiled::{CompiledMatch, CompiledRoute, Matcher, RouteSet};
use crate::raw::{RawRoute, RawRouteFile, RawRouteMatch};

#[derive(Debug, Error)]
pub enum RouteError {
    #[error("route #{index} ({name:?}) has an empty name")]
    EmptyName { index: usize, name: String },

    #[error(
        "route #{index} ({name:?}) has an empty match block — set `any = true` for an unconditional match"
    )]
    EmptyMatch { index: usize, name: String },

    #[error(
        "route #{index} ({name:?}) sets `any = true` together with other match keys; \
         `any` must be exclusive"
    )]
    AnyWithOtherKeys { index: usize, name: String },

    #[error("route #{index} ({name:?}) match key {key:?} is not a valid regex: {err}")]
    BadRegex {
        index: usize,
        name: String,
        key: String,
        err: regex::Error,
    },

    #[error("route #{index} ({name:?}) declares duplicate header {header:?}")]
    DuplicateHeader {
        index: usize,
        name: String,
        header: String,
    },

    #[error("two routes share name {name:?} (#{first} and #{second}); names must be unique")]
    DuplicateName {
        name: String,
        first: usize,
        second: usize,
    },

    #[error("route #{index} ({name:?}) [route.bridge.tls] is invalid: {source}")]
    BadBridgeTls {
        index: usize,
        name: String,
        #[source]
        source: siphon_ai_bridge::tls::TlsConfigError,
    },
}

/// Compile a deserialized route file into a ready-to-evaluate
/// [`RouteSet`].
///
/// Returns the first error encountered — config loading is
/// fail-loud at startup per CLAUDE.md §4.6, so there's no point
/// collecting multiple errors.
pub fn compile(file: RawRouteFile) -> Result<RouteSet, RouteError> {
    // Reject duplicate names up front. Names appear in metrics
    // labels and HEP correlation, so collisions would silently
    // poison observability.
    for (i, r) in file.routes.iter().enumerate() {
        for (j, other) in file.routes.iter().enumerate().take(i) {
            if r.name == other.name {
                return Err(RouteError::DuplicateName {
                    name: r.name.clone(),
                    first: j,
                    second: i,
                });
            }
        }
    }

    let mut compiled = Vec::with_capacity(file.routes.len());
    for (idx, route) in file.routes.into_iter().enumerate() {
        compiled.push(compile_one(idx, route)?);
    }
    Ok(RouteSet::from_routes(compiled))
}

fn compile_one(index: usize, route: RawRoute) -> Result<CompiledRoute, RouteError> {
    if route.name.trim().is_empty() {
        return Err(RouteError::EmptyName {
            index,
            name: route.name.clone(),
        });
    }

    let RawRoute {
        name,
        match_,
        bridge,
        media,
        security,
        recording,
    } = route;

    let compiled_match = compile_match(index, &name, match_)?;
    let bridge_tls = compile_bridge_tls(index, &name, &bridge)?;

    Ok(CompiledRoute {
        name,
        match_: compiled_match,
        bridge,
        media,
        security,
        recording,
        bridge_tls,
    })
}

/// Load `[route.bridge.tls]` (cert/key from disk, optional pin) into a
/// `BridgeTlsConfig` at config-compile time, so a bad path/cert fails
/// loud at startup rather than on the first matching call. `None` when
/// the route didn't set the block (it then inherits the global
/// `[bridge.tls]` at accept time).
fn compile_bridge_tls(
    index: usize,
    name: &str,
    bridge: &crate::raw::BridgeOverride,
) -> Result<Option<siphon_ai_bridge::tls::BridgeTlsConfig>, RouteError> {
    let Some(tls) = bridge.tls.as_ref() else {
        return Ok(None);
    };
    let cfg = siphon_ai_bridge::tls::BridgeTlsConfig::from_paths(
        std::path::Path::new(&tls.client_cert),
        std::path::Path::new(&tls.client_key),
        tls.pinned_sha256.as_deref(),
    )
    .map_err(|source| RouteError::BadBridgeTls {
        index,
        name: name.to_string(),
        source,
    })?;
    Ok(Some(cfg))
}

fn compile_match(
    index: usize,
    name: &str,
    raw: RawRouteMatch,
) -> Result<CompiledMatch, RouteError> {
    let RawRouteMatch {
        any,
        regex,
        request_uri_user,
        request_uri_host,
        to_user,
        to_host,
        from_user,
        from_host,
        register_source,
        header,
    } = raw;

    let has_other_keys = request_uri_user.is_some()
        || request_uri_host.is_some()
        || to_user.is_some()
        || to_host.is_some()
        || from_user.is_some()
        || from_host.is_some()
        || register_source.is_some()
        || !header.is_empty();

    if any && has_other_keys {
        return Err(RouteError::AnyWithOtherKeys {
            index,
            name: name.to_string(),
        });
    }
    if !any && !has_other_keys {
        return Err(RouteError::EmptyMatch {
            index,
            name: name.to_string(),
        });
    }

    let pred = |key: &str, value: Option<String>| -> Result<Option<Matcher>, RouteError> {
        match value {
            None => Ok(None),
            Some(v) => Ok(Some(build_matcher(index, name, key, v, regex)?)),
        }
    };

    // Normalize header name lower-case here so the matcher can
    // compare in O(1) without ever lowercasing again.
    let mut headers = Vec::with_capacity(header.len());
    let mut seen_lower: Vec<String> = Vec::with_capacity(header.len());
    for (header_name, header_value) in header {
        let lower = header_name.to_ascii_lowercase();
        if seen_lower.contains(&lower) {
            return Err(RouteError::DuplicateHeader {
                index,
                name: name.to_string(),
                header: header_name,
            });
        }
        let pred = build_matcher(
            index,
            name,
            &format!("header.{header_name}"),
            header_value,
            regex,
        )?;
        seen_lower.push(lower.clone());
        headers.push((lower, pred));
    }

    Ok(CompiledMatch {
        any,
        request_uri_user: pred("request_uri_user", request_uri_user)?,
        request_uri_host: pred("request_uri_host", request_uri_host)?,
        to_user: pred("to_user", to_user)?,
        to_host: pred("to_host", to_host)?,
        from_user: pred("from_user", from_user)?,
        from_host: pred("from_host", from_host)?,
        register_source: pred("register_source", register_source)?,
        headers,
    })
}

fn build_matcher(
    index: usize,
    name: &str,
    key: &str,
    value: String,
    is_regex: bool,
) -> Result<Matcher, RouteError> {
    if is_regex {
        regex::Regex::new(&value)
            .map(Matcher::Regex)
            .map_err(|err| RouteError::BadRegex {
                index,
                name: name.to_string(),
                key: key.to_string(),
                err,
            })
    } else {
        // Pre-lowercase so the matcher can do byte-wise compares
        // without per-call allocations.
        Ok(Matcher::Literal(value.to_ascii_lowercase()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raw::{BridgeOverride, MediaOverride, RecordingOverride, SecurityOverride};

    fn raw_with_match(name: &str, m: RawRouteMatch) -> RawRoute {
        RawRoute {
            name: name.into(),
            match_: m,
            bridge: BridgeOverride::default(),
            media: MediaOverride::default(),
            security: SecurityOverride::default(),
            recording: RecordingOverride::default(),
        }
    }

    #[test]
    fn empty_match_is_rejected() {
        let f = RawRouteFile {
            routes: vec![raw_with_match("r", RawRouteMatch::default())],
        };
        assert!(matches!(compile(f), Err(RouteError::EmptyMatch { .. })));
    }

    #[test]
    fn any_with_other_keys_is_rejected() {
        let f = RawRouteFile {
            routes: vec![raw_with_match(
                "r",
                RawRouteMatch {
                    any: true,
                    request_uri_user: Some("5000".into()),
                    ..Default::default()
                },
            )],
        };
        assert!(matches!(
            compile(f),
            Err(RouteError::AnyWithOtherKeys { .. })
        ));
    }

    #[test]
    fn duplicate_route_name_is_rejected() {
        let f = RawRouteFile {
            routes: vec![
                raw_with_match(
                    "r",
                    RawRouteMatch {
                        request_uri_user: Some("a".into()),
                        ..Default::default()
                    },
                ),
                raw_with_match(
                    "r",
                    RawRouteMatch {
                        request_uri_user: Some("b".into()),
                        ..Default::default()
                    },
                ),
            ],
        };
        assert!(matches!(compile(f), Err(RouteError::DuplicateName { .. })));
    }

    #[test]
    fn empty_name_is_rejected() {
        let f = RawRouteFile {
            routes: vec![raw_with_match(
                "  ",
                RawRouteMatch {
                    request_uri_user: Some("a".into()),
                    ..Default::default()
                },
            )],
        };
        assert!(matches!(compile(f), Err(RouteError::EmptyName { .. })));
    }

    #[test]
    fn bad_regex_is_rejected() {
        let f = RawRouteFile {
            routes: vec![raw_with_match(
                "r",
                RawRouteMatch {
                    regex: true,
                    request_uri_user: Some("[bad".into()),
                    ..Default::default()
                },
            )],
        };
        let err = compile(f).unwrap_err();
        match err {
            RouteError::BadRegex { key, .. } => assert_eq!(key, "request_uri_user"),
            other => panic!("expected BadRegex, got {other:?}"),
        }
    }

    fn fixture(name: &str) -> String {
        format!("{}/src/testdata/{name}", env!("CARGO_MANIFEST_DIR"))
    }

    fn raw_with_bridge(name: &str, bridge: BridgeOverride) -> RawRoute {
        RawRoute {
            name: name.into(),
            match_: RawRouteMatch {
                any: true,
                ..Default::default()
            },
            bridge,
            media: MediaOverride::default(),
            security: SecurityOverride::default(),
            recording: RecordingOverride::default(),
        }
    }

    #[test]
    fn route_without_tls_compiles_to_none() {
        let set = compile(RawRouteFile {
            routes: vec![raw_with_bridge("r", BridgeOverride::default())],
        })
        .expect("compiles");
        assert!(set.iter().next().unwrap().bridge_tls.is_none());
    }

    #[test]
    fn route_with_valid_tls_compiles_to_some() {
        use crate::raw::BridgeTlsOverride;
        let bridge = BridgeOverride {
            tls: Some(BridgeTlsOverride {
                client_cert: fixture("client_cert.pem"),
                client_key: fixture("client_key.pem"),
                pinned_sha256: None,
            }),
            ..Default::default()
        };
        let set = compile(RawRouteFile {
            routes: vec![raw_with_bridge("secure", bridge)],
        })
        .expect("compiles with valid cert/key");
        assert!(
            set.iter().next().unwrap().bridge_tls.is_some(),
            "valid [route.bridge.tls] should compile to Some"
        );
    }

    #[test]
    fn route_with_bad_tls_path_is_rejected() {
        use crate::raw::BridgeTlsOverride;
        let bridge = BridgeOverride {
            tls: Some(BridgeTlsOverride {
                client_cert: "/nonexistent/cert.pem".into(),
                client_key: "/nonexistent/key.pem".into(),
                pinned_sha256: None,
            }),
            ..Default::default()
        };
        let err = compile(RawRouteFile {
            routes: vec![raw_with_bridge("secure", bridge)],
        })
        .unwrap_err();
        assert!(
            matches!(err, RouteError::BadBridgeTls { ref name, .. } if name == "secure"),
            "expected BadBridgeTls, got {err:?}"
        );
    }

    #[test]
    fn duplicate_header_is_rejected_case_insensitively() {
        let f = RawRouteFile {
            routes: vec![raw_with_match(
                "r",
                RawRouteMatch {
                    header: [
                        ("X-Customer-Id".to_string(), "a".to_string()),
                        ("x-customer-id".to_string(), "b".to_string()),
                    ]
                    .into_iter()
                    .collect(),
                    ..Default::default()
                },
            )],
        };
        assert!(matches!(
            compile(f),
            Err(RouteError::DuplicateHeader { .. })
        ));
    }
}
