//! 自己改善ドラフト設定の既定値・読み込み・範囲検証。

use std::path::Path;

use config::{Config, ConfigError, LoadOptions, SelfImprovementConfig};

fn options(directory: &Path) -> LoadOptions {
    LoadOptions {
        project_dir: Some(directory.to_path_buf()),
        user_config_dir: Some(directory.join("empty-user")),
        read_env: false,
        ..Default::default()
    }
}

#[test]
fn defaults_are_disabled_with_documented_limits_and_collectors() {
    let expected = SelfImprovementConfig {
        enabled: false,
        draft_dir: None,
        max_candidates: 200,
        evidence_max_bytes: 2048,
        daily_limit: 20,
        duplicate_cooldown_secs: 86_400,
        collect_diagnostics: true,
        collect_lessons: true,
    };
    assert_eq!(Config::default().self_improvement, expected);
    expected.validate().expect("valid defaults");

    // Given: the section or its fields are omitted / When: deserializing / Then: default-off.
    for document in ["version = 2\n", "[self_improvement]\n"] {
        let config: Config = toml::from_str(document).expect("defaults");
        assert_eq!(config.self_improvement, expected);
    }
    let directory = tempfile::tempdir().expect("temp");
    assert_eq!(
        Config::load(&options(directory.path()))
            .expect("load defaults")
            .self_improvement,
        expected
    );
    let serialized = toml::to_string(&expected).expect("serialize defaults");
    assert!(!serialized.contains("draft_dir"));
}

#[test]
fn populated_section_parses_and_round_trips() {
    // Given: every field has an explicit value differing from its default.
    let document = r#"
[self_improvement]
enabled = true
draft_dir = "local/drafts"
max_candidates = 500
evidence_max_bytes = 4096
daily_limit = 30
duplicate_cooldown_secs = 172800
collect_diagnostics = false
collect_lessons = false
"#;
    let expected = SelfImprovementConfig {
        enabled: true,
        draft_dir: Some("local/drafts".into()),
        max_candidates: 500,
        evidence_max_bytes: 4096,
        daily_limit: 30,
        duplicate_cooldown_secs: 172_800,
        collect_diagnostics: false,
        collect_lessons: false,
    };

    // When: parsing directly, serializing and loading via either public load API.
    let parsed: Config = toml::from_str(document).expect("parse");
    let serialized = toml::to_string(&parsed).expect("serialize");
    let reparsed: Config = toml::from_str(&serialized).expect("reparse");
    assert_eq!(parsed.self_improvement, expected);
    assert_eq!(reparsed, parsed);
    let directory = tempfile::tempdir().expect("temp");
    std::fs::create_dir_all(directory.path().join(".evorch")).expect("config directory");
    std::fs::write(directory.path().join(".evorch/config.toml"), document).expect("write");
    for load in [Config::load, Config::load_strict] {
        assert_eq!(
            load(&options(directory.path()))
                .expect("load populated section")
                .self_improvement,
            expected
        );
    }
}

#[test]
fn both_load_paths_reject_out_of_range_values_even_when_disabled() {
    let directory = tempfile::tempdir().expect("temp");
    std::fs::create_dir_all(directory.path().join(".evorch")).expect("config directory");
    for (field, minimum, maximum) in [
        ("max_candidates", 1, 10_000),
        ("evidence_max_bytes", 256, 65_536),
        ("daily_limit", 1, 1_000),
        ("duplicate_cooldown_secs", 60, 31_536_000),
    ] {
        for value in [minimum - 1, maximum + 1] {
            // Given: an out-of-range field, with enabled omitted (false).
            let document = format!("[self_improvement]\n{field} = {value}\n");
            std::fs::write(directory.path().join(".evorch/config.toml"), &document).expect("write");
            let section: Config = toml::from_str(&document).expect("typed value");
            let direct_error = section.self_improvement.validate().expect_err("invalid");

            // When: validating directly or loading / Then: reject, never clamp.
            for error in [direct_error].into_iter().chain(
                [Config::load, Config::load_strict]
                    .map(|load| load(&options(directory.path())).expect_err("out of range")),
            ) {
                match error {
                    ConfigError::InvalidField { path, message } => {
                        assert_eq!(path, format!("self_improvement.{field}"));
                        assert!(message.contains(&format!("{minimum}..={maximum}")));
                    }
                    other => panic!("expected field validation error, got {other}"),
                }
            }
        }
    }
}

#[test]
fn both_load_paths_accept_inclusive_range_boundaries() {
    let directory = tempfile::tempdir().expect("temp");
    std::fs::create_dir_all(directory.path().join(".evorch")).expect("config directory");
    for (field, boundaries) in [
        ("max_candidates", [1, 10_000]),
        ("evidence_max_bytes", [256, 65_536]),
        ("daily_limit", [1, 1_000]),
        ("duplicate_cooldown_secs", [60, 31_536_000]),
    ] {
        for value in boundaries {
            std::fs::write(
                directory.path().join(".evorch/config.toml"),
                format!("[self_improvement]\n{field} = {value}\n"),
            )
            .expect("write");
            for load in [Config::load, Config::load_strict] {
                let loaded = load(&options(directory.path())).expect("inclusive boundary");
                let serialized = toml::Value::try_from(loaded).expect("serialize");
                assert_eq!(
                    serialized["self_improvement"][field].as_integer(),
                    Some(value)
                );
            }
        }
    }
}

#[test]
fn validation_uses_merged_values_in_both_load_paths() {
    let directory = tempfile::tempdir().expect("temp");
    std::fs::create_dir_all(directory.path().join(".evorch")).expect("config directory");
    std::fs::write(
        directory.path().join(".evorch/config.toml"),
        "[self_improvement]\ndaily_limit = 0\nmax_candidates = 300\n",
    )
    .expect("write");
    let mut options = options(directory.path());
    options.cli_overrides =
        Some(toml::from_str("[self_improvement]\ndaily_limit = 5\n").expect("CLI override"));
    for load in [Config::load, Config::load_strict] {
        let loaded = load(&options).expect("valid merged value overrides invalid lower layer");
        assert_eq!(loaded.self_improvement.daily_limit, 5);
        assert_eq!(loaded.self_improvement.max_candidates, 300);
    }
    options.cli_overrides =
        Some(toml::from_str("[self_improvement]\ndaily_limit = 1001\n").expect("CLI override"));
    for load in [Config::load, Config::load_strict] {
        assert!(matches!(
            load(&options),
            Err(ConfigError::InvalidField { path, .. }) if path == "self_improvement.daily_limit"
        ));
    }
}
