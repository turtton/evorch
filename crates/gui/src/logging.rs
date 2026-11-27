use tracing_subscriber::EnvFilter;

/// 画像添付 (v05) 有効化まで、`install_image_loaders` の空 WARN を抑止する。
const QUIET_MODULES: &str = ",egui_extras::loaders=error";

pub fn log_filter_from_env(value: Option<&str>) -> EnvFilter {
    let base = value.unwrap_or("info");
    let composed = format!("{base}{QUIET_MODULES}");
    EnvFilter::try_new(composed).unwrap_or_else(|_| EnvFilter::new(format!("info{QUIET_MODULES}")))
}

pub fn env_filter() -> EnvFilter {
    log_filter_from_env(std::env::var("RUST_LOG").ok().as_deref())
}

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

static LOGGER: OnceLock<Logger<RotatingLog>> = OnceLock::new();

/// The only targets saved to the log file: how the connection to the core was
/// lost and recovered. The file outlives the process, and other crates log what
/// they were given (an external checker's stderr can hold credentials), so a
/// target is saved by name, not by level.
pub fn saved_targets() -> Targets {
    Targets::new()
        .with_target("gui::core_client::connection", Level::INFO)
        .with_target("gui::core_client::disconnect_log", Level::INFO)
        .with_target("workbench_client", Level::INFO)
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

/// stderr (everything `RUST_LOG` allows), and `gui-<pid>.log` next to the core's
/// `core-<pid>.log` (see `core_instance::log_file`): a window opened from a
/// launcher has no terminal, and the line that explains a lost core connection
/// must still be readable afterwards. The file is written by a thread of its
/// own; a log file that cannot be opened leaves stderr only.
pub fn init() {
    let logger = log_dir()
        .and_then(|dir| RotatingLog::open_for_process(&dir, "gui", MAX_LOG_BYTES).ok())
        .map(|file| Logger::new(file, QUEUE_LINES));
    let file = logger.as_ref().map(|logger| {
        let sink = logger.sink();
        file_layer(move || sink.make_writer())
    });
    tracing_subscriber::registry()
        .with(file)
        .with(
            fmt::layer()
                .with_writer(std::io::stderr)
                .with_filter(env_filter()),
        )
        .init();
    if let Some(logger) = logger {
        logger.start();
        let _ = LOGGER.set(logger);
    }
}

/// Write what is queued, when the process is about to end. Bounded.
pub fn flush() {
    if let Some(logger) = LOGGER.get() {
        logger.flush();
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn log_filter_defaults_to_info_when_env_missing() {
        // Given: no environment override. When: resolving the filter. Then: info is enabled with egui_extras noise suppressed.
        let filter = super::log_filter_from_env(None).to_string();
        assert!(filter.contains("info"));
        assert!(filter.contains("egui_extras::loaders=error"));
    }

    #[test]
    fn log_filter_respects_env_override() {
        // Given: debug override. When: resolving the filter. Then: debug is enabled with noise suppression.
        let filter = super::log_filter_from_env(Some("debug")).to_string();
        assert!(filter.contains("debug"));
        assert!(filter.contains("egui_extras::loaders=error"));
    }

    #[test]
    fn log_filter_falls_back_to_info_when_env_invalid() {
        // Given: an invalid directive. When: resolving the filter. Then: fallback is info with noise suppression.
        let filter = super::log_filter_from_env(Some("[=")).to_string();
        assert!(filter.contains("info"));
        assert!(filter.contains("egui_extras::loaders=error"));
    }

    #[derive(Clone, Default)]
    struct Captured(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for Captured {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn the_log_file_keeps_connection_diagnostics_and_nothing_another_crate_logged() {
        use tracing_subscriber::layer::SubscriberExt;
        // Given: the file layer, as installed.
        let log = Captured::default();
        let writer = log.clone();
        let subscriber =
            tracing_subscriber::registry().with(super::file_layer(move || writer.clone()));
        // When: the GUI, the client and an unrelated crate log at warn level.
        tracing::subscriber::with_default(subscriber, || {
            tracing::warn!(target: "gui::core_client::connection", "lost the connection");
            tracing::warn!(target: "workbench_client::transport", "core socket I/O failed");
            tracing::warn!(target: "tools::post_edit", stderr = "token=sk-live-123", "external checker failed");
        });
        // Then: only the connection diagnostics are written.
        let text = String::from_utf8(log.0.lock().unwrap().clone()).unwrap();
        assert!(
            text.contains("lost the connection") && text.contains("core socket I/O failed"),
            "{text}"
        );
        assert!(
            !text.contains("sk-live") && !text.contains("external checker"),
            "{text}"
        );
    }
}
