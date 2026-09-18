use config::{Config, LoadOptions};

#[test]
fn sandbox_allow_network_defaults_to_false_when_section_absent() {
    // Given: a configuration without a sandbox section.
    let config: Config = toml::from_str("version = 2").expect("config");
    // When: inspecting the serialized configuration boundary.
    let value = toml::Value::try_from(config).expect("serialize");
    // Then: the default is explicitly false.
    assert_eq!(
        value
            .get("sandbox")
            .and_then(|v| v.get("allow_network"))
            .and_then(toml::Value::as_bool),
        Some(false)
    );
}

#[test]
fn sandbox_allow_network_round_trips_when_true() {
    // Given: an explicit global opt-in on disk.
    let dir = tempfile::tempdir().expect("temp");
    std::fs::write(
        dir.path().join("evorch.toml"),
        "[sandbox]\nallow_network = true\n",
    )
    .expect("write");
    // When: loading through the strict boundary and serializing again.
    let config = Config::load(&LoadOptions {
        project_dir: Some(dir.path().into()),
        user_config_dir: Some(dir.path().join("user")),
        read_env: false,
        ..Default::default()
    })
    .expect("sandbox setting accepted");
    let text = toml::to_string(&config).expect("serialize");
    let reparsed: Config = toml::from_str(&text).expect("reparse");
    // Then: the opt-in survives the round trip.
    assert_eq!(config, reparsed);
    let value = toml::Value::try_from(reparsed).expect("value");
    assert_eq!(value["sandbox"]["allow_network"].as_bool(), Some(true));
}
