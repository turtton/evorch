use config::Config;

#[test]
fn named_roles_resolve_category_overrides() {
    for role in ["planner", "oracle", "multimodal_looker"] {
        let doc = format!(
            "[agents.roles.{role}]\nlogical_model = 'role-model'\n[agents.roles.{role}.categories.visual]\nlogical_model = 'visual-model'\n"
        );
        let config: Config = toml::from_str(&doc).expect("closed named role binding");
        assert_eq!(
            config.agents.binding_for(role, None).unwrap().logical_model,
            "role-model"
        );
        assert_eq!(
            config
                .agents
                .binding_for(role, Some("visual"))
                .unwrap()
                .logical_model,
            "visual-model"
        );
    }
}

#[test]
fn named_roles_reject_unknown_registry_entries() {
    assert!(toml::from_str::<Config>("[agents.roles.custom]\nlogical_model = 'worker'").is_err());
}

#[test]
fn layered_loading_validates_closed_role_bindings() {
    // Given: real layered loading with no ambient user configuration.
    let directory = tempfile::tempdir().expect("temporary config directory");
    for (document, expected_path) in [
        ("[agents.roles.planner]\nlogical_model = 'plan'", None),
        (
            "[agents.roles.custom]\nlogical_model = 'plan'",
            Some("agents.roles.custom"),
        ),
        (
            "[agents.roles.oracle]\nunknown = true",
            Some("agents.roles.oracle.unknown"),
        ),
        (
            "[agents.roles.multimodal_looker.categories.typo]\nlogical_model = 'vision'",
            Some("agents.roles.multimodal_looker.categories.typo"),
        ),
    ] {
        // When: the merged configuration crosses the strict boundary.
        let result = Config::load(&config::LoadOptions {
            user_config_dir: Some(directory.path().to_path_buf()),
            read_env: false,
            cli_overrides: Some(toml::from_str(document).expect("TOML")),
            ..Default::default()
        });
        // Then: only the fixed roles and known binding fields are accepted.
        match expected_path {
            None => assert_eq!(
                result
                    .expect("valid binding")
                    .agents
                    .binding_for("planner", None)
                    .expect("planner")
                    .logical_model,
                "plan"
            ),
            Some(expected) => assert!(
                matches!(result, Err(config::ConfigError::InvalidField { path, .. }) if path == expected)
            ),
        }
    }
}
