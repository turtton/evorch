use config::Config;

#[test]
fn team_is_disabled_when_omitted() {
    // Given / When: an existing config without a team section.
    let config: Config = toml::from_str("").expect("config");
    // Then: opt-in is required.
    assert!(!config.team.enabled);
    assert_eq!(config.team.max_workers, 3);
}

#[test]
fn team_can_be_enabled_explicitly() {
    // Given / When: explicit team configuration.
    let config: Config =
        toml::from_str("[team]\nenabled = true\nmax_workers = 2").expect("team config");
    // Then: the requested bounded parallelism is retained.
    assert!(config.team.enabled);
    assert_eq!(config.team.max_workers, 2);
}

#[test]
fn team_rejects_unbounded_worker_counts() {
    // Given: invalid worker limits.
    for count in [0, 4, 255] {
        // When / Then: parsing rejects the limit.
        assert!(toml::from_str::<Config>(&format!("[team]\nmax_workers = {count}")).is_err());
    }
}
