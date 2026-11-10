use config::{Config, ConfigError, LoadOptions};

fn load(content: &str, strict: bool) -> Result<Config, ConfigError> {
    let directory = tempfile::tempdir().expect("temporary directory");
    std::fs::write(directory.path().join("config.toml"), content).expect("write config");
    let options = LoadOptions {
        user_config_dir: Some(directory.path().to_path_buf()),
        read_env: false,
        ..Default::default()
    };
    if strict {
        Config::load_strict(&options)
    } else {
        Config::load(&options)
    }
}

const DOCUMENT: &str = r#"
[eval]
repeat_target = 4
obsolete = true
[eval.budget]
max_tool_calls = 9
obsolete = true
[eval.judge]
models = [{ route = "judge", obsolete = true }]
obsolete = true
[eval.judge.generation]
temperature = 0.0
obsolete = true
"#;

#[test]
fn eval_unknown_keys_are_rejected_strictly_and_pruned_tolerantly() {
    // Given: an eval section with an unknown key at every nesting level.

    // When: loading strictly, and then tolerantly.
    let strict = load(DOCUMENT, true);
    let tolerant = load(DOCUMENT, false).expect("unknown keys are ignored");

    // Then: strict loading names the first unknown path; tolerant loading keeps known values.
    match strict {
        Err(ConfigError::InvalidField { path, .. }) => assert!(path.starts_with("eval.")),
        other => panic!("expected an invalid eval field, got {other:?}"),
    }
    assert_eq!(tolerant.eval.repeat_target, 4);
    assert_eq!(tolerant.eval.budget.max_tool_calls, 9);
    let judge = tolerant.eval.judge.expect("judge binding");
    assert_eq!(judge.head_route(), Some("judge"));
    assert_eq!(judge.generation.temperature, Some(0.0));
}

#[test]
fn eval_bindings_accept_the_role_binding_shape() {
    // Given: a simulator binding with a chain, preset and generation overrides.
    let document = r#"
[eval.user_simulator]
models = [{ route = "cheap", reasoning_effort = "low" }]
preset = "plain"
[eval.user_simulator.generation]
max_tokens = 512
"#;

    // When: it is loaded strictly.
    let config = load(document, true).expect("valid binding");

    // Then: every field is kept.
    let simulator = config.eval.user_simulator.expect("simulator binding");
    assert_eq!(simulator.head_route(), Some("cheap"));
    assert_eq!(simulator.models[0].reasoning_effort.as_deref(), Some("low"));
    assert_eq!(simulator.preset.as_deref(), Some("plain"));
    assert_eq!(simulator.generation.max_tokens, Some(512));
}
