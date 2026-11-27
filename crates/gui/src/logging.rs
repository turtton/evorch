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

/// stderr, and `gui.log` next to the core's `core.log` (see `core_instance::log_file`):
/// a window opened from a launcher has no terminal, and the line that explains a
/// lost core connection must still be readable afterwards. A log file that cannot
/// be opened leaves stderr only.
pub fn init() {
    use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
    let file = core_instance::log_file::log_dir().and_then(|dir| {
        core_instance::log_file::RotatingLog::open(
            &dir,
            "gui",
            core_instance::log_file::MAX_LOG_BYTES,
        )
        .ok()
    });
    let file_layer = file.map(|file| {
        tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_writer(std::sync::Mutex::new(file))
    });
    tracing_subscriber::registry()
        .with(env_filter())
        .with(file_layer)
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .init();
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
}
