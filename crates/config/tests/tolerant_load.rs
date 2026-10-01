use config::{Config, ConfigError, LoadOptions};

fn load(content: &str, strict: bool) -> Result<Config, ConfigError> {
    let directory = tempfile::tempdir().expect("temporary directory");
    std::fs::create_dir_all(directory.path().join(".evorch")).expect("config directory");
    std::fs::write(directory.path().join(".evorch/config.toml"), content).expect("write config");
    let options = LoadOptions {
        project_dir: Some(directory.path().to_path_buf()),
        user_config_dir: Some(directory.path().join("empty-user")),
        read_env: false,
        ..Default::default()
    };
    if strict {
        Config::load_strict(&options)
    } else {
        Config::load(&options)
    }
}

#[test]
fn diagnostic_persistence_is_strictly_validated_and_defaults_to_warnings() {
    use config::DiagnosticPersistence;
    assert_eq!(
        load("", true).unwrap().diagnostics.persistence,
        DiagnosticPersistence::Warnings
    );
    for (value, expected) in [
        ("off", DiagnosticPersistence::Off),
        ("warnings", DiagnosticPersistence::Warnings),
        ("all", DiagnosticPersistence::All),
    ] {
        let document = format!("[diagnostics]\npersistence = '{value}'\n");
        assert_eq!(
            load(&document, true).unwrap().diagnostics.persistence,
            expected
        );
        assert_eq!(
            load(&document, false).unwrap().diagnostics.persistence,
            expected
        );
    }
    assert!(load("[diagnostics]\npersistence = 'warnngs'\n", true).is_err());
    assert!(load("[diagnostics]\npersistence = 'warnngs'\n", false).is_err());
}

#[test]
fn unknown_fields_do_not_discard_valid_provider_and_routing() {
    let document = r#"
version = 2
[providers.primary]
type = "openai-compatible"
base_url = "https://example.com/v1"
credential = { type = "env", var = "EXAMPLE_API_KEY", obsolete = true }
models = [{ id = "model-a", enabled = true, old_property = "ignored" }]
default_model = "model-a"
timeout = 30
[agents.roles.librarian]
logical_model = "retired"
[agents.roles.web_researcher]
logical_model = "research"
[agents.worker]
logical_model = "worker-model"
old_property = true
[agents.worker.categories.qucik]
logical_model = "ignored"
[[routing.routes.worker-model]]
profile = "primary"
model = "model-a"
weight = 2
[diagnostics]
log_level = "debug"
obsolete = "ignored"
"#;

    let config = load(document, false).expect("unknown fields should not abort loading");
    assert!(config.providers.contains_key("primary"));
    assert_eq!(config.providers["primary"].default_model, "model-a");
    assert_eq!(config.routing.routes["worker-model"][0].profile, "primary");
    assert_eq!(
        config.agents.worker.logical_model.as_deref(),
        Some("worker-model")
    );
    assert_eq!(
        config.agents.roles.web_researcher.logical_model.as_deref(),
        Some("research")
    );
    assert!(load(document, true).is_err());
}

#[test]
fn self_improvement_unknown_keys_are_pruned_like_metrics_and_compaction() {
    // Given: ordinary sections contain both supported fields and obsolete keys.
    let document = r#"
[metrics]
retention_days = 7
obsolete = true
[compaction]
keep_recent_tokens = 1234
obsolete = true
[self_improvement]
enabled = true
daily_limit = 8
obsolete = true
"#;

    // When: loading with the same tolerant policy used for existing sections.
    let config = load(document, false).expect("unknown keys are ignored");

    // Then: pruning preserves the supported values in every section.
    assert_eq!(config.metrics.retention_days, 7);
    assert_eq!(config.compaction.keep_recent_tokens, 1234);
    assert!(config.self_improvement.enabled);
    assert_eq!(config.self_improvement.daily_limit, 8);
    let disabled = load("[self_improvement]\nenabld = true\n", false).expect("typo ignored");
    assert!(!disabled.self_improvement.enabled);
}

#[test]
fn known_field_with_wrong_type_still_fails() {
    let error = load("[budget]\nmax_tool_calls = 'many'\n", false).unwrap_err();
    assert!(error.to_string().contains("max_tool_calls"));
}

#[test]
fn plaintext_credential_still_fails() {
    let error = load("[providers.primary]\napi_key = 'secret'\n", false).unwrap_err();
    assert!(
        matches!(error, ConfigError::InvalidField { path, .. } if path == "providers.primary.api_key")
    );
}

#[test]
fn tolerant_category_pruning_preserves_internal_worker_bindings() {
    let document = r#"
[agents.worker.categories.quick]
logical_model = "quick-model"
[agents.worker.categories.lesson]
logical_model = "lesson-model"
[agents.worker.categories.lesson_review]
logical_model = "wrong-role-model"
[agents.worker.categories.unknown]
logical_model = "unknown-model"
"#;

    let config = load(document, false).expect("invalid categories are ignored");

    assert_eq!(config.agents.worker.categories.len(), 2);
    assert_eq!(
        config
            .agents
            .binding_for("worker", Some("quick"))
            .unwrap()
            .logical_model,
        "quick-model"
    );
    assert_eq!(
        config
            .agents
            .binding_for("worker", Some("lesson"))
            .unwrap()
            .logical_model,
        "lesson-model"
    );
    assert!(
        !config
            .agents
            .worker
            .categories
            .contains_key("lesson_review")
    );
    assert!(!config.agents.worker.categories.contains_key("unknown"));
}

#[test]
fn tolerant_category_pruning_preserves_reviewer_bindings_and_known_fields() {
    let document = r#"
[agents.reviewer]
logical_model = "review-base"
[agents.reviewer.categories.plan]
logical_model = "plan-model"
unknown = "ignored"
[agents.reviewer.categories.plan.generation]
max_tokens = 1024
seed = 42
[agents.reviewer.categories.tool-execution]
preset = "tool-preset"
[agents.reviewer.categories.lesson_review]
logical_model = "lesson-model"
[agents.reviewer.categories.quick]
logical_model = "wrong-role"
[agents.reviewer.categories.unknown]
logical_model = "unknown-model"
"#;
    let config = load(document, false).expect("invalid category fields are ignored");
    assert_eq!(config.agents.reviewer.categories.len(), 3);
    let plan = config.agents.binding_for("reviewer", Some("plan")).unwrap();
    assert_eq!(plan.logical_model, "plan-model");
    assert_eq!(plan.generation.max_tokens, Some(1024));
    let tool = config
        .agents
        .binding_for("reviewer", Some("tool-execution"))
        .unwrap();
    assert_eq!(tool.logical_model, "review-base");
    assert_eq!(tool.preset.as_deref(), Some("tool-preset"));
    assert_eq!(
        config.agents.reviewer.categories["lesson_review"]
            .logical_model
            .as_deref(),
        Some("lesson-model")
    );
    assert!(!config.agents.reviewer.categories.contains_key("quick"));
    assert!(!config.agents.reviewer.categories.contains_key("unknown"));
}
