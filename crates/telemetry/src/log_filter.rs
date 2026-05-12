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
}

impl LogFilterHandle {
    /// Construct from a `tracing_subscriber::reload::Handle`.
    /// Usually called by `init_tracing` in the daemon binary.
    pub fn new(inner: reload::Handle<EnvFilter, tracing_subscriber::Registry>) -> Self {
        Self {
            inner: Arc::new(inner),
        }
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
    pub fn set(&self, directive: &str) -> Result<String, LogFilterError> {
        let prev = self.current();
        let new = EnvFilter::try_new(directive).map_err(|e| LogFilterError::Parse {
            directive: directive.to_string(),
            err: e.to_string(),
        })?;
        self.inner
            .reload(new)
            .map_err(|e| LogFilterError::Reload(e.to_string()))?;
        Ok(prev)
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
