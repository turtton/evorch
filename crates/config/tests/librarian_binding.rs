use config::{AgentsConfig, Config, ConfigError, LoadOptions};

#[test]
fn librarian_defaults_when_binding_is_absent() {
    // Given: the default agent configuration.
    let agents = AgentsConfig::default();
    // When: resolving the research role without a category.
    let binding = agents.binding_for("librarian", None).expect("librarian");
    // Then: the role-name fallback applies.
    assert_eq!(binding.logical_model, "librarian");
}

#[test]
fn old_toml_parses_when_librarian_is_absent() {
    // Given: all seven previously supported bindings.
    let document = "[agents.orchestrator]\n[agents.explorer]\n[agents.worker]\n[agents.reviewer]\n[agents.roles.planner]\n[agents.roles.oracle]\n[agents.roles.multimodal_looker]\n";
    // When: deserializing the old document.
    let config: Config = toml::from_str(document).expect("backward compatible TOML");
    // Then: the previous defaults remain unchanged.
    assert_eq!(config.agents, AgentsConfig::default());
}

#[test]
fn librarian_explicit_binding_resolves_when_configured() {
    // Given: an explicit librarian model.
    let config: Config =
        toml::from_str("[agents.roles.librarian]\nlogical_model = 'research-model'")
            .expect("librarian schema");
    // When: resolving the role.
    let binding = config
        .agents
        .binding_for("librarian", None)
        .expect("binding");
    // Then: the explicit assignment wins over the role-name fallback.
    assert_eq!(binding.logical_model, "research-model");
}

#[test]
fn librarian_rejects_category_and_generation_when_loaded() {
    // Given: distinct role and category bindings with generation inheritance.
    let overrides = toml::from_str(
        r#"
[agents.roles.librarian]
logical_model = "research-model"
preset = "research-preset"
[agents.roles.librarian.generation]
temperature = 0.25
top_p = 0.8
max_tokens = 1024
reasoning_effort = "low"
[agents.roles.librarian.categories.research]
logical_model = "deep-model"
preset = "deep-preset"
[agents.roles.librarian.categories.research.generation]
max_tokens = 4096
reasoning_effort = "high"
"#,
    )
    .expect("TOML");
    let directory = tempfile::tempdir().expect("temp");
    // When: loading through strict validation.
    let error = Config::load(&LoadOptions {
        user_config_dir: Some(directory.path().join("user")),
        read_env: false,
        cli_overrides: Some(overrides),
        ..Default::default()
    })
    .expect_err("librarian categories must be rejected");
    // Then: the category field itself is rejected before deserialization.
    assert!(
        matches!(&error, ConfigError::InvalidField { path, .. }
            if path == "agents.roles.librarian.categories"),
        "{error}"
    );
}
