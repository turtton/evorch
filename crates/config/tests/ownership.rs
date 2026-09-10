use config::Config;

#[test]
fn ownership_window_when_configured() {
    // Given / When: explicit local-process lease settings.
    let config: Config =
        toml::from_str("[ownership]\nheartbeat_ms = 100\nlease_ms = 500\ngrace_ms = 1000\n")
            .expect("ownership section");
    // Then: the section survives parsing rather than being ignored.
    assert!(
        toml::to_string(&config)
            .expect("serialize")
            .contains("heartbeat_ms = 100")
    );
}
