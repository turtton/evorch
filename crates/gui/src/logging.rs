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
        assert_eq!(super::log_filter_from_env(Some("debug")).to_string(), "debug");
    }

    #[test]
    fn log_filter_defaults_to_info_when_env_invalid() {
        // Given: an invalid directive. When: resolving the filter. Then: fallback is info.
        assert_eq!(super::log_filter_from_env(Some("[=")).to_string(), "info");
    }
}
