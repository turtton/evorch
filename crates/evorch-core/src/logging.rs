//! The core's own log: why a connection ended, and everything else the core
//! reports through `tracing`. Before this existed the core had no subscriber, so
//! every `tracing` line was dropped and a lost connection left no trace.
//!
//! The lines go to `<state dir>/evorch/logs/core.log` (see
//! [`core_instance::log_file`]). `RUST_LOG` overrides the default filter. When the
//! file cannot be opened the core logs to stderr instead and still starts.
use core_instance::log_file::{MAX_LOG_BYTES, RotatingLog, log_dir};
use std::sync::Mutex;
use tracing_subscriber::EnvFilter;

const LOG_NAME: &str = "core";

fn filter() -> EnvFilter {
    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"))
}

/// Install the process-wide subscriber. Call once, early; a second call (or a
/// subscriber installed by an embedder) is ignored.
pub fn init() {
    init_in(log_dir());
}

/// [`init`] with the log directory chosen by the caller.
pub fn init_in(dir: Option<std::path::PathBuf>) {
    let opened = dir.ok_or("no state directory").and_then(|dir| {
        RotatingLog::open(&dir, LOG_NAME, MAX_LOG_BYTES).map_err(|_| "cannot open the log file")
    });
    let builder = tracing_subscriber::fmt().with_env_filter(filter());
    let _ = match opened {
        Ok(file) => builder
            .with_ansi(false)
            .with_writer(Mutex::new(file))
            .try_init(),
        Err(_) => builder.with_writer(std::io::stderr).try_init(),
    };
    tracing::info!(
        pid = std::process::id(),
        version = env!("CARGO_PKG_VERSION"),
        "evorch-core logging started"
    );
}
