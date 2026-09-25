use config::{Config, EscalationApproval, LoadOptions, SandboxConfig, save_sandbox};

#[test]
fn web_tools_default_to_enabled_and_round_trip_disabled() {
    for input in [
        "version = 2",
        "[sandbox]",
        "[sandbox]\nescalation_approval = \"user\"",
    ] {
        let defaults: Config = toml::from_str(input).expect("defaults");
        assert!(defaults.sandbox.web_tools_enabled);
    }

    let dir = tempfile::tempdir().expect("temp");
    let path = dir.path().join("evorch.toml");
    save_sandbox(
        &path,
        SandboxConfig {
            web_tools_enabled: false,
            ..Default::default()
        },
    )
    .expect("save");
    let loaded = Config::load(&LoadOptions {
        project_dir: Some(dir.path().into()),
        user_config_dir: Some(dir.path().join("user")),
        read_env: false,
        ..Default::default()
    })
    .expect("load");
    assert!(!loaded.sandbox.web_tools_enabled);
    let saved = std::fs::read_to_string(&path).expect("read");
    assert!(saved.contains("web_tools_enabled = false"));
    assert!(!saved.contains("allow_network"));
    assert!(!saved.contains("web_tool_access"));
}

#[test]
fn removed_network_settings_are_rejected_at_the_strict_boundary() {
    for setting in ["allow_network = true", "web_tool_access = \"opt-in\""] {
        let dir = tempfile::tempdir().expect("temp");
        std::fs::write(
            dir.path().join("evorch.toml"),
            format!("[sandbox]\n{setting}\n"),
        )
        .expect("write");
        let error = Config::load_strict(&LoadOptions {
            project_dir: Some(dir.path().into()),
            user_config_dir: Some(dir.path().join("user")),
            read_env: false,
            ..Default::default()
        })
        .expect_err("removed keys must be rejected");
        assert!(
            error
                .to_string()
                .contains(setting.split(' ').next().unwrap())
        );
    }
}

#[test]
fn escalation_approval_defaults_to_auto_when_section_absent() {
    // Given: a configuration without a sandbox section.
    let config: Config = toml::from_str("version = 2").expect("config");

    // When: inspecting the default sandbox settings.
    let sandbox = config.sandbox;

    // Then: escalation approval is Auto and deny escalation is disabled.
    assert_eq!(sandbox.escalation_approval, EscalationApproval::Auto);
    assert!(!sandbox.escalate_to_user_on_deny);
}

#[test]
fn approval_mode_deserializes_auto_and_legacy_quick_alias() {
    // Given: current and legacy on-disk approval values.
    let auto: Config = toml::from_str("[sandbox]\nescalation_approval = \"auto\"").expect("auto");
    let legacy: Config =
        toml::from_str("[sandbox]\nescalation_approval = \"quick\"").expect("legacy quick");

    // When: serializing the parsed values.
    let serialized = toml::to_string(&auto).expect("serialize");

    // Then: both values select Auto and serialization emits the current name.
    assert_eq!(auto.sandbox.escalation_approval, EscalationApproval::Auto);
    assert_eq!(legacy.sandbox.escalation_approval, EscalationApproval::Auto);
    let serialized_config: Config = toml::from_str(&serialized).expect("serialized TOML");
    assert_eq!(
        serialized_config.sandbox.escalation_approval,
        EscalationApproval::Auto
    );
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
        web_tools_enabled: false,
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
