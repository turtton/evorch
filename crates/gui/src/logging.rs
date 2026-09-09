use tracing_subscriber::EnvFilter;

pub fn log_filter_from_env(value: Option<&str>) -> EnvFilter {
    value
        .and_then(|value| EnvFilter::try_new(value).ok())
        .unwrap_or_else(|| EnvFilter::new("info"))
}

pub fn env_filter() -> EnvFilter {
    log_filter_from_env(std::env::var("RUST_LOG").ok().as_deref())
}

pub fn init() {
    tracing_subscriber::fmt()
        .with_env_filter(env_filter())
        .with_writer(std::io::stderr)
        .init();
}

#[cfg(test)]
mod tests {
    #[test]
    fn log_filter_defaults_to_info_when_env_missing() {
        // Given: no environment override. When: resolving the filter. Then: info is enabled.
        assert_eq!(super::log_filter_from_env(None).to_string(), "info");
    }

    #[test]
    fn log_filter_respects_env_override() {
        // Given: debug override. When: resolving the filter. Then: debug is enabled.
        assert_eq!(
            super::log_filter_from_env(Some("debug")).to_string(),
            "debug"
        );
    }

    #[test]
    fn log_filter_defaults_to_info_when_env_invalid() {
        // Given: an invalid directive. When: resolving the filter. Then: fallback is info.
        assert_eq!(super::log_filter_from_env(Some("[=")).to_string(), "info");
    }
}
