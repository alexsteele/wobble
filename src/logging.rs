//! Process-wide tracing setup for CLI and server execution.
//!
//! The CLI uses a small stderr-only subscriber, while the long-running server
//! uses a separate initializer that always writes to a daily-rolled log file
//! under the node-home `logs/` directory and can optionally mirror the same
//! events to stderr. Both paths share the same compact formatter and
//! `RUST_LOG` filtering rules.

use std::{fs, path::Path, sync::{Once, OnceLock}};

use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{EnvFilter, fmt};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

static INIT_LOGGING: Once = Once::new();
static FILE_LOG_GUARD: OnceLock<WorkerGuard> = OnceLock::new();

/// Installs the standard stderr-only tracing subscriber for short-lived CLI commands.
///
/// The default filter is `info`. Callers can override it with `RUST_LOG`, for
/// example `RUST_LOG=wobble=debug`.
pub fn init_cli_tracing() {
    INIT_LOGGING.call_once(|| {
        let filter =
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
        tracing_subscriber::registry()
            .with(filter)
            .with(
                fmt::layer()
                    .with_writer(std::io::stderr)
                    .with_target(false)
                    .with_thread_ids(false)
                    .with_thread_names(false)
                    .compact(),
            )
            .init();
    });
}

/// Installs the server tracing subscriber with daily file logging and optional stderr mirroring.
pub fn init_server_tracing(log_dir: &Path, log_stderr: bool) {
    INIT_LOGGING.call_once(|| {
        if let Err(err) = fs::create_dir_all(log_dir) {
            eprintln!("log directory create failed for {}: {err}", log_dir.display());
        }
        let filter =
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
        let file_appender = tracing_appender::rolling::daily(log_dir, "server.log");
        let (file_writer, guard) = tracing_appender::non_blocking(file_appender);
        let _ = FILE_LOG_GUARD.set(guard);
        let registry = tracing_subscriber::registry().with(filter).with(
            fmt::layer()
                .with_ansi(false)
                .with_writer(file_writer)
                .with_target(false)
                .with_thread_ids(false)
                .with_thread_names(false)
                .compact(),
        );

        if log_stderr {
            registry
                .with(
                    fmt::layer()
                        .with_writer(std::io::stderr)
                        .with_target(false)
                        .with_thread_ids(false)
                        .with_thread_names(false)
                        .compact(),
                )
                .init();
        } else {
            registry.init();
        }
    });
}
