//! The core's own log: why a connection ended, and what the core's lifecycle
//! reports. Before this existed the core had no subscriber, so every `tracing`
//! line was dropped and a lost connection left no trace.
//!
//! Lines go to `<state dir>/evorch/logs/core-<pid>.log` (see
//! [`core_instance::log_file`]), written by a thread of their own so that
//! logging never waits for the disk on the task that logs. When the file cannot
//! be opened the lines go to stderr and the core still starts.
//!
//! Only the targets in [`saved_targets`] are kept. The log is a file that
//! outlives the process and other crates log what they were given, such as the
//! stderr of an external checker, which can hold credentials.
use core_instance::log_file::{Logger, MAX_LOG_BYTES, QUEUE_LINES, RotatingLog, log_dir};
use std::sync::OnceLock;
use tracing::Level;
use tracing_subscriber::{
    Layer,
    filter::Targets,
    fmt::{self, MakeWriter},
    layer::SubscriberExt,
    registry::LookupSpan,
    util::SubscriberInitExt,
};

const LOG_NAME: &str = "core";

static LOGGER: OnceLock<Logger<RotatingLog>> = OnceLock::new();

/// The only targets whose lines are saved: the connections and the lifecycle of
/// this crate. The lines name ids, error kinds and I/O errors, not request or
/// tool content.
pub fn saved_targets() -> Targets {
    Targets::new()
        .with_target("evorch_core::connection_log", Level::INFO)
        .with_target("evorch_core::lifecycle", Level::INFO)
        .with_target("evorch_core::logging", Level::INFO)
}

/// A layer that writes the saved targets, in plain text, to `writer`.
pub fn file_layer<S>(writer: impl for<'w> MakeWriter<'w> + 'static) -> impl Layer<S>
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
{
    fmt::layer()
        .with_ansi(false)
        .with_writer(writer)
        .with_filter(saved_targets())
}

/// Install the process-wide subscriber. Call once, early; a second call (or a
/// subscriber installed by an embedder) is ignored. Nothing is written until
/// [`start`]: the entry of a planned successor must stay single-threaded until
/// its descriptors are adopted, so the writing thread starts later.
pub fn init() {
    init_in(log_dir());
}

/// [`init`] with the log directory chosen by the caller.
pub fn init_in(dir: Option<std::path::PathBuf>) {
    let opened =
        dir.and_then(|dir| RotatingLog::open_for_process(&dir, LOG_NAME, MAX_LOG_BYTES).ok());
    match opened {
        Some(file) => {
            let logger = Logger::new(file, QUEUE_LINES);
            let sink = logger.sink();
            let installed = tracing_subscriber::registry()
                .with(file_layer(move || sink.make_writer()))
                .try_init()
                .is_ok();
            if installed {
                let _ = LOGGER.set(logger);
            }
        }
        None => {
            let _ = tracing_subscriber::registry()
                .with(
                    fmt::layer()
                        .with_writer(std::io::stderr)
                        .with_filter(saved_targets()),
                )
                .try_init();
        }
    }
    tracing::info!(
        pid = std::process::id(),
        version = env!("CARGO_PKG_VERSION"),
        "evorch-core logging started"
    );
}

/// Start the thread that writes the queued lines. Idempotent.
pub fn start() {
    if let Some(logger) = LOGGER.get() {
        logger.start();
    }
}

/// Write what is queued, when the process is about to end. Bounded.
pub fn flush() {
    if let Some(logger) = LOGGER.get() {
        logger.flush();
    }
}
