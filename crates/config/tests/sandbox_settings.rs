use config::{Config, EscalationApproval, LoadOptions, SandboxConfig, save_sandbox};

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

#[test]
fn escalation_approval_defaults_to_quick_when_section_absent() {
    // Given: a configuration without a sandbox section.
    let config: Config = toml::from_str("version = 2").expect("config");

    // When: inspecting the default sandbox settings.
    let sandbox = config.sandbox;

    // Then: escalation approval is Quick and deny escalation is disabled.
    assert_eq!(sandbox.escalation_approval, EscalationApproval::Quick);
    assert!(!sandbox.escalate_to_user_on_deny);
}

#[test]
fn escalation_approval_parses_user_and_off_values() {
    // Given: each kebab-case escalation approval value on disk.
    for (value, expected) in [
        ("user", EscalationApproval::User),
        ("off", EscalationApproval::Off),
    ] {
        let text = format!("[sandbox]\nescalation_approval = \"{value}\"\n");

        // When: parsing and serializing the configuration.
        let config: Config = toml::from_str(&text).expect("config");
        let serialized = toml::to_string(&config).expect("serialize");
        let reparsed: Config = toml::from_str(&serialized).expect("reparse");

        // Then: the selected value survives the round trip.
        assert_eq!(reparsed.sandbox.escalation_approval, expected);
    }
}

#[test]
fn escalate_to_user_on_deny_defaults_to_false_and_round_trips() {
    // Given: deny escalation enabled in the sandbox section.
    let text = "[sandbox]\nescalate_to_user_on_deny = true\n";

    // When: parsing and serializing the configuration.
    let config: Config = toml::from_str(text).expect("config");
    let serialized = toml::to_string(&config).expect("serialize");
    let reparsed: Config = toml::from_str(&serialized).expect("reparse");

    // Then: the explicit value survives and omission remains false by default.
    assert!(reparsed.sandbox.escalate_to_user_on_deny);
    let defaults: Config = toml::from_str("version = 2").expect("config");
    assert!(!defaults.sandbox.escalate_to_user_on_deny);
}

#[test]
fn save_sandbox_persists_escalation_fields_and_keeps_other_sections() {
    // Given: a document containing an unrelated provider section.
    let tmp = tempfile::tempdir().expect("temp");
    let path = tmp.path().join("evorch.toml");
    std::fs::write(
        &path,
        "version = 2\n\n[providers.keep]\nprovider_type = \"openai\"\n",
    )
    .expect("write");
    let sandbox = SandboxConfig {
        allow_network: true,
        escalation_approval: EscalationApproval::User,
        escalate_to_user_on_deny: true,
    };

    // When: saving only the sandbox section and reloading the document.
    save_sandbox(&path, sandbox).expect("save sandbox");
    let config = Config::load(&LoadOptions {
        project_dir: Some(tmp.path().to_path_buf()),
        user_config_dir: Some(tmp.path().join("empty")),
        read_env: false,
        ..LoadOptions::default()
    })
    .expect("reload");

    // Then: both new fields and the unrelated provider remain intact.
    assert_eq!(config.sandbox, sandbox);
    assert!(config.providers.contains_key("keep"));
}
