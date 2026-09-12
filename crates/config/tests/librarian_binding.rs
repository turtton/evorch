use config::{AgentsConfig, Config, LoadOptions, save_agent_bindings};

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
fn librarian_roundtrips_when_saved_with_category_and_generation() {
    // Given: distinct role and category bindings with generation inheritance.
    let config: Config = toml::from_str(
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
    .expect("librarian schema");
    let directory = tempfile::tempdir().expect("temp");
    // When: saving through the strict save API and loading from disk.
    save_agent_bindings(&directory.path().join("evorch.toml"), &config.agents).expect("save");
    let loaded = Config::load(&LoadOptions {
        project_dir: Some(directory.path().into()),
        user_config_dir: Some(directory.path().join("user")),
        read_env: false,
        ..Default::default()
    })
    .expect("load");
    // Then: every field survives and category overrides inherit role generation.
    assert_eq!(loaded.agents, config.agents);
    let binding = loaded
        .agents
        .binding_for("librarian", Some("research"))
        .expect("binding");
    assert_eq!(binding.logical_model, "deep-model");
    assert_eq!(binding.preset.as_deref(), Some("deep-preset"));
    assert_eq!(binding.generation.temperature, Some(0.25));
    assert_eq!(binding.generation.top_p, Some(0.8));
    assert_eq!(binding.generation.max_tokens, Some(4096));
    assert_eq!(
        binding.generation.reasoning_effort,
        Some(config::ReasoningEffortConfig::High)
    );
}
