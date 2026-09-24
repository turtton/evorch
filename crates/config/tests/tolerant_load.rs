use config::{Config, ConfigError, LoadOptions};

fn load(content: &str, strict: bool) -> Result<Config, ConfigError> {
    let directory = tempfile::tempdir().expect("temporary directory");
    std::fs::write(directory.path().join("evorch.toml"), content).expect("write config");
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
