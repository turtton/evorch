use config::{AgentsConfig, Config, ConfigError, LoadOptions};

#[test]
fn web_researcher_defaults_when_binding_is_absent() {
    // Given: the default agent configuration.
    let agents = AgentsConfig::default();
    // When: resolving the research role without a category.
    let binding = agents
        .binding_for("web_researcher", None)
        .expect("web_researcher");
    // Then: the role-name fallback applies.
    assert_eq!(binding.logical_model, "web_researcher");
}

#[test]
fn config_defaults_when_web_researcher_is_absent() {
    // Given: a configuration omitting the optional research binding.
    let document = "[agents.orchestrator]\n[agents.explorer]\n[agents.worker]\n[agents.reviewer]\n[agents.roles.planner]\n[agents.roles.oracle]\n[agents.roles.multimodal_looker]\n";
    // When: deserializing the document.
    let config: Config = toml::from_str(document).expect("optional role binding");
    // Then: omitted bindings resolve to their defaults.
    assert_eq!(config.agents, AgentsConfig::default());
}

#[test]
fn web_researcher_explicit_binding_resolves_when_configured() {
    // Given: an explicit web_researcher model.
    let config: Config =
        toml::from_str("[agents.roles.web_researcher]\nlogical_model = 'research-model'")
            .expect("web_researcher schema");
    // When: resolving the role.
    let binding = config
        .agents
        .binding_for("web_researcher", None)
        .expect("binding");
    // Then: the explicit assignment wins over the role-name fallback.
    assert_eq!(binding.logical_model, "research-model");
}

#[test]
fn web_researcher_rejects_category_when_loaded() {
    // Given: distinct role and category bindings with generation inheritance.
    let overrides = toml::from_str(
        r#"
[agents.roles.web_researcher]
logical_model = "research-model"
preset = "research-preset"
[agents.roles.web_researcher.generation]
temperature = 0.25
top_p = 0.8
max_tokens = 1024
reasoning_effort = "low"
[agents.roles.web_researcher.categories.research]
logical_model = "deep-model"
preset = "deep-preset"
[agents.roles.web_researcher.categories.research.generation]
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
    .expect_err("web_researcher categories must be rejected");
    // Then: the category field itself is rejected before deserialization.
    assert!(
        matches!(&error, ConfigError::InvalidField { path, .. }
            if path == "agents.roles.web_researcher.categories"),
        "{error}"
    );
}

#[test]
fn retired_role_binding_is_rejected() {
    let error = toml::from_str::<Config>("[agents.roles.librarian]")
        .expect_err("retired role key is not an alias");
    assert!(error.to_string().contains("librarian"));
}

#[test]
fn web_researcher_baseline_and_appendix_resolve_through_config() {
    let mut config = Config::default();
    config.agents.roles.web_researcher.preset = Some("category-research".into());
    let sources = config::resolve_prompt_sources(&config, None).expect("research prompt sources");
    assert_eq!(sources.role_baselines.len(), 8);
    assert!(sources.role_baselines["webresearcher"].contains("# WebResearcher"));
    assert_eq!(
        sources.appendices["category-research"],
        config::presets::PresetStore::resolve("category-research", None).expect("appendix")
    );
}
