//! Library surface for the `siphon-ai` daemon.
//!
//! The binary at `src/main.rs` is a thin shell over [`Runtime`]; the
//! library form exists so integration tests in `tests/` can build
//! the same runtime without spawning a child process.

pub mod registration;
pub mod runtime;

pub use runtime::Runtime;
