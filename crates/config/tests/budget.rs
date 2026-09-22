use config::Config;

#[test]
fn defaults_when_budget_is_missing() {
    // Given: a config without a budget table.
    let document = "version = 2";
    // When: parsed and serialized through the public configuration type.
    let config: Config = toml::from_str(document).unwrap();
    let value = serde_json::to_value(config).unwrap();
    // Then: existing defaults and the repeat limit are present.
    assert_eq!(
        value["budget"],
        serde_json::json!({
            "max_tool_calls": 400, "max_no_progress_rounds": 100,
            "max_file_rereads": 20, "max_identical_tool_call_repeats": 5,
            "max_tokens": 2_000_000, "max_elapsed_secs": 7_200
        })
    );
}

#[test]
fn defaults_merge_when_budget_is_partial() {
    // Given: one overridden budget.
    let document = "[budget]\nmax_tool_calls = 17";
    // When: parsed as the root config.
    let config: Config = toml::from_str(document).unwrap();
    let value = serde_json::to_value(config).unwrap();
    // Then: omitted settings retain their defaults.
    assert_eq!(
        value["budget"],
        serde_json::json!({
            "max_tool_calls": 17, "max_no_progress_rounds": 100,
            "max_file_rereads": 20, "max_identical_tool_call_repeats": 5,
            "max_tokens": 2_000_000, "max_elapsed_secs": 7_200
        })
    );
}

#[test]
fn rejects_unknown_budget_key() {
    // Given: a misspelled key in the budget table.
    let document = "[budget]\nmax_identical_tool_call_repeat = 2";
    // When: parsed as the root config.
    let error = toml::from_str::<Config>(document).unwrap_err();
    // Then: rejection identifies the nested key, not the budget section.
    assert!(
        error
            .to_string()
            .contains("unknown field `max_identical_tool_call_repeat`")
    );
}

#[test]
fn parses_identical_repeat_limit() {
    // Given: an explicit repeat threshold.
    let document = "[budget]\nmax_identical_tool_call_repeats = 2";
    // When: parsed as the root config.
    let config: Config = toml::from_str(document).unwrap();
    // Then: the configured value survives parsing.
    assert_eq!(
        serde_json::to_value(config).unwrap()["budget"]["max_identical_tool_call_repeats"],
        2
    );
}

#[test]
fn loads_budget_from_project_file() {
    // Given: an isolated project config with budget overrides.
    let project = tempfile::tempdir().unwrap();
    let user = tempfile::tempdir().unwrap();
    std::fs::write(
        project.path().join("evorch.toml"),
        "version = 2\n[budget]\nmax_tool_calls = 37\nmax_identical_tool_call_repeats = 2",
    )
    .unwrap();
    // When: the real layered loader applies strict validation and defaults.
    let config = Config::load(&config::LoadOptions {
        project_dir: Some(project.path().into()),
        user_config_dir: Some(user.path().into()),
        read_env: false,
        ..Default::default()
    })
    .unwrap();
    // Then: both overrides and omitted defaults survive the loading path.
    assert_eq!(
        config.budget,
        config::BudgetConfig {
            max_tool_calls: 37,
            max_identical_tool_call_repeats: 2,
            ..Default::default()
        }
    );
}

#[test]
fn loads_cumulative_token_and_elapsed_limits() {
    let project = tempfile::tempdir().unwrap();
    let user = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("evorch.toml"),
        "version = 2\n[budget]\nmax_tokens = 7654321\nmax_elapsed_secs = 1234\n[compaction]\nsummary_idle_timeout_secs = 45\nsummary_timeout_secs = 180\nfailure_cooldown_turns = 6").unwrap();
    let config = Config::load(&config::LoadOptions {
        project_dir: Some(project.path().into()),
        user_config_dir: Some(user.path().into()),
        read_env: false,
        ..Default::default()
    })
    .unwrap();
    assert_eq!(config.budget.max_tokens, 7_654_321);
    assert_eq!(config.budget.max_elapsed_secs, 1234);
    assert_eq!(config.compaction.summary_idle_timeout_secs, 45);
    assert_eq!(config.compaction.summary_timeout_secs, 180);
    assert_eq!(config.compaction.failure_cooldown_turns, 6);
}
