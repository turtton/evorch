use config::Config;

#[test]
fn ownership_window_when_configured() {
    // Given / When: explicit local-process lease settings.
    let config: Config =
        toml::from_str("[ownership]\nheartbeat_ms = 100\nlease_ms = 500\ngrace_ms = 1000\n")
            .expect("ownership section");
    // Then: every explicit lease setting survives parsing and serialization.
    assert_eq!(config.ownership.heartbeat_ms.get(), 100);
    assert_eq!(config.ownership.lease_ms.get(), 500);
    assert_eq!(config.ownership.grace_ms.get(), 1000);
    let serialized = toml::to_string(&config).expect("serialize");
    assert_eq!(
        toml::from_str::<Config>(&serialized).expect("round trip"),
        config
    );
}
