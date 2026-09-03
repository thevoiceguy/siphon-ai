//! Dynamic log-filter handle.
//!
//! `tracing` doesn't ship a built-in way to swap the filter at
//! runtime — the supported pattern is `tracing_subscriber::reload`,
//! which gives you a `Handle` you can call `.reload(new_layer)` on.
//! We wrap that handle here so the admin HTTP endpoint can flip the
//! filter without re-implementing the reload dance every time it's
//! used.
//!
//! The daemon's `main` builds the subscriber + reload handle and
//! hands the [`LogFilterHandle`] to the runtime; the admin endpoint
//! borrows it for `PUT /admin/log`.

use std::sync::Arc;

use thiserror::Error;
use tracing_subscriber::reload;
use tracing_subscriber::EnvFilter;

/// Reload handle wrapper. Clone-on-demand; the inner `reload::Handle`
/// is itself cheap to clone (it's an `Arc` under the hood).
#[derive(Clone)]
pub struct LogFilterHandle {
    inner: Arc<reload::Handle<EnvFilter, tracing_subscriber::Registry>>,
    /// Whether [`set`](Self::set) prepends the `warn` floor to a directive
    /// that carries no global level (#597). `true` unless the daemon was
    /// started with `--log-no-floor`.
    enforce_floor: bool,
}

/// The global level prepended to a directive that has none (#597).
pub const LOG_FLOOR: &str = "warn";

/// `true` when `directive` carries a **global** level — a bare level
/// token such as `warn` or `info` (any case) anywhere in the
/// comma-separated list. Every other directive shape (`target=level`,
/// `target[span]=level`, `[span{field}]=level`) is target- or
/// span-scoped and leaves unnamed targets muted.
pub fn has_global_level(directive: &str) -> bool {
    directive.split(',').map(str::trim).any(|d| {
        !d.is_empty()
            && !d.contains('=')
            && !d.contains('[')
            && matches!(
                d.to_ascii_lowercase().as_str(),
                "off" | "error" | "warn" | "info" | "debug" | "trace"
            )
    })
}

/// Prepend the `warn` floor to `directive` unless it already carries a
/// global level (#597). Returns the effective directive and whether the
/// floor was added.
///
/// The floor is what makes a filter fail-safe: a crate nobody named —
/// `hep_rs` reporting a dead collector, `opentelemetry_sdk` reporting a
/// failed export — can still get a warning out. Without this, any
/// operator filter that names targets (`siphon_ai=debug`) silently
/// deleted that guarantee, which is how two collector outages went
/// unlogged (#460, #596). A directive that *names* a global level
/// (`info,siphon_ai=debug`, or a bare `off`) is left alone: the operator
/// has said what the floor is.
pub fn with_floor(directive: &str) -> (String, bool) {
    let trimmed = directive.trim();
    if trimmed.is_empty() {
        return (LOG_FLOOR.to_string(), true);
    }
    if has_global_level(trimmed) {
        (trimmed.to_string(), false)
    } else {
        (format!("{LOG_FLOOR},{trimmed}"), true)
    }
}

impl LogFilterHandle {
    /// Construct from a `tracing_subscriber::reload::Handle`.
    /// Usually called by `init_tracing` in the daemon binary.
    pub fn new(inner: reload::Handle<EnvFilter, tracing_subscriber::Registry>) -> Self {
        Self {
            inner: Arc::new(inner),
            enforce_floor: true,
        }
    }

    /// Whether [`set`](Self::set) enforces the `warn` floor. The daemon
    /// passes `false` only for `--log-no-floor`.
    pub fn with_enforce_floor(mut self, enforce: bool) -> Self {
        self.enforce_floor = enforce;
        self
    }

    /// `true` when runtime filter changes get the floor prepended.
    pub fn enforces_floor(&self) -> bool {
        self.enforce_floor
    }

    /// Build a handle wired to a fresh, no-effect reload layer.
    ///
    /// Useful in tests where the daemon's `Runtime::build` requires
    /// a `LogFilterHandle` but the test doesn't actually exercise
    /// the admin endpoint. `current()` returns the default filter
    /// string; `set()` succeeds but doesn't affect any real
    /// subscriber.
    pub fn noop() -> Self {
        let filter = EnvFilter::new("off");
        let (_layer, handle) =
            reload::Layer::<EnvFilter, tracing_subscriber::Registry>::new(filter);
        Self::new(handle)
    }

    /// Read back the current filter directive as a string.
    ///
    /// Used by the admin endpoint's GET so operators can see what's
    /// active without guessing.
    pub fn current(&self) -> String {
        // EnvFilter's Display impl produces the canonical directive
        // string. `with_current` borrows the layer immutably.
        self.inner
            .with_current(|f| f.to_string())
            .unwrap_or_else(|_| String::from("<unavailable>"))
    }

    /// Swap the filter to a new directive string. Returns the
    /// previous directive on success; `Err` if `directive` doesn't
    /// parse.
    ///
    /// Unless the handle was built with `with_enforce_floor(false)`, a
    /// directive with no global level gets the `warn` floor prepended
    /// first (see [`with_floor`]) — the same rule the daemon applies to
    /// `--log` / `RUST_LOG` at startup, so `PUT /admin/v1/log` cannot
    /// mute unnamed crates by accident either. Use [`set_effective`]
    /// (Self::set_effective) to learn what was actually installed.
    pub fn set(&self, directive: &str) -> Result<String, LogFilterError> {
        self.set_effective(directive).map(|(prev, _)| prev)
    }

    /// [`set`](Self::set), also returning the directive actually
    /// installed (floored or not).
    pub fn set_effective(&self, directive: &str) -> Result<(String, String), LogFilterError> {
        let effective = if self.enforce_floor {
            with_floor(directive).0
        } else {
            directive.trim().to_string()
        };
        let prev = self.current();
        let new = EnvFilter::try_new(&effective).map_err(|e| LogFilterError::Parse {
            directive: directive.to_string(),
            err: e.to_string(),
        })?;
        self.inner
            .reload(new)
            .map_err(|e| LogFilterError::Reload(e.to_string()))?;
        Ok((prev, effective))
    }
}

/// Errors surfaced by [`LogFilterHandle::set`]. Returned over the
/// admin API as a 4xx (bad directive) or 5xx (reload internals).
#[derive(Debug, Error)]
pub enum LogFilterError {
    /// `directive` didn't parse as a valid `EnvFilter` string —
    /// caller error.
    #[error("invalid log directive {directive:?}: {err}")]
    Parse { directive: String, err: String },

    /// Reload itself failed — almost always means the subscriber
    /// was dropped, which is fatal-ish for the daemon.
    #[error("filter reload failed: {0}")]
    Reload(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_only_directives_get_the_floor() {
        for d in [
            "siphon_ai=debug",
            "siphon_ai=info,siphon=info,forge=info",
            "siphon_ai::registration=debug",
            "siphon_ai[call{call_id}]=trace",
            "[span{field=1}]=debug",
        ] {
            let (eff, added) = with_floor(d);
            assert!(added, "{d}");
            assert_eq!(eff, format!("warn,{d}"));
            assert!(!has_global_level(d));
        }
    }

    #[test]
    fn a_global_level_anywhere_is_left_alone() {
        for d in [
            "warn,siphon_ai=debug",
            "siphon_ai=debug,warn",
            "info",
            "OFF",
            "Debug,hyper=off",
            " error , siphon_ai=trace ",
        ] {
            let (eff, added) = with_floor(d);
            assert!(!added, "{d}");
            assert_eq!(eff, d.trim());
            assert!(has_global_level(d));
        }
    }

    #[test]
    fn empty_becomes_the_bare_floor() {
        assert_eq!(with_floor(""), ("warn".to_string(), true));
        assert_eq!(with_floor("   "), ("warn".to_string(), true));
    }

    #[test]
    fn level_lookalikes_do_not_count_as_a_floor() {
        // A target that happens to be spelled like a level is still a
        // target directive, and a level-valued target is not global.
        assert!(!has_global_level("warn=debug"));
        assert!(!has_global_level("info[span]=trace"));
        assert!(!has_global_level("warning"));
    }

    #[test]
    fn handle_set_applies_the_floor_unless_disabled() {
        let (_layer, handle) =
            reload::Layer::<EnvFilter, tracing_subscriber::Registry>::new(EnvFilter::new("off"));
        let h = LogFilterHandle::new(handle);
        let (_, eff) = h.set_effective("siphon_ai=debug").unwrap();
        assert_eq!(eff, "warn,siphon_ai=debug");
        let (_, eff) = h.set_effective("off").unwrap();
        assert_eq!(eff, "off");

        let (_layer, handle) =
            reload::Layer::<EnvFilter, tracing_subscriber::Registry>::new(EnvFilter::new("off"));
        let h = LogFilterHandle::new(handle).with_enforce_floor(false);
        let (_, eff) = h.set_effective("siphon_ai=debug").unwrap();
        assert_eq!(eff, "siphon_ai=debug");
    }
}
