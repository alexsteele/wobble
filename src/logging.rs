//! Process-wide tracing setup for CLI and server execution.
//!
//! This module installs a small `tracing-subscriber` formatter so the binary
//! emits consistent, field-based logs to stderr. Filtering follows `RUST_LOG`
//! and falls back to `info` when no explicit filter is provided.

use std::sync::Once;

use tracing_subscriber::{EnvFilter, fmt};

static INIT_LOGGING: Once = Once::new();

/// Installs the global tracing subscriber once for the current process.
///
/// The default filter is `info`. Callers can override it with `RUST_LOG`, for
/// example `RUST_LOG=wobble=debug`.
pub fn init() {
    INIT_LOGGING.call_once(|| {
        let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
        fmt()
            .with_env_filter(filter)
            .with_target(false)
            .with_thread_ids(false)
            .with_thread_names(false)
            .compact()
            .init();
    });
}
