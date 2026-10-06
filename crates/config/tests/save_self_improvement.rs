//! 自己改善ドラフト設定の安全な保存。

use std::path::Path;

use config::{
    Config, ConfigError, LoadOptions, SandboxConfig, SelfImprovementConfig, save_sandbox,
    save_self_improvement,
};

fn load(directory: &Path) -> Config {
    Config::load_strict(&LoadOptions {
        user_config_dir: Some(directory.join(".evorch")),
        read_env: false,
        ..Default::default()
    })
    .expect("strict reload")
}

#[test]
fn save_replaces_only_self_improvement_and_preserves_unrelated_bytes() {
    let directory = tempfile::tempdir().expect("temp");
    std::fs::create_dir_all(directory.path().join(".evorch")).expect("config directory");
    let path = directory.path().join(".evorch/config.toml");
    let before = "# user config\nversion = 2\n\n[providers.keep] # provider comment\nprovider_type = 'openai'\n\n[diagnostics]\nlog_level  = 'debug' # keep spacing\n";
    let after = "\n[metrics] # metrics comment\nenabled = false\nretention_days = 90\n";
    std::fs::write(
        &path,
        format!("{before}\n[self_improvement]\nenabled = false\ndraft_dir = 'old'\n{after}"),
    )
    .expect("write");
    let mut expected = load(directory.path());
    expected.self_improvement = SelfImprovementConfig {
        enabled: true,
        draft_dir: Some("drafts/new".into()),
        max_candidates: 400,
        evidence_max_bytes: 4096,
        daily_limit: 50,
        duplicate_cooldown_secs: 172_800,
        collect_diagnostics: false,
        collect_lessons: false,
    };

    save_self_improvement(&path, &expected.self_improvement).expect("save");

    let saved = std::fs::read_to_string(&path).expect("read");
    assert!(
        saved.starts_with(before),
        "unrelated prefix changed: {saved}"
    );
    assert!(saved.ends_with(after), "unrelated suffix changed: {saved}");
    assert_eq!(load(directory.path()), expected);
    assert!(!path.with_extension("toml.tmp").exists());
}

#[test]
fn save_defaults_creates_a_file_and_removes_a_previous_draft_dir() {
    let directory = tempfile::tempdir().expect("temp");
    std::fs::create_dir_all(directory.path().join(".evorch")).expect("config directory");
    let path = directory.path().join(".evorch/config.toml");
    let defaults = SelfImprovementConfig::default();
    save_self_improvement(&path, &defaults).expect("create");
    assert_eq!(load(directory.path()), Config::default());
    let with_dir = SelfImprovementConfig {
        draft_dir: Some("old/drafts".into()),
        ..Default::default()
    };
    save_self_improvement(&path, &with_dir).expect("set directory");
    assert_eq!(load(directory.path()).self_improvement, with_dir);

    save_self_improvement(&path, &defaults).expect("reset");

    assert_eq!(load(directory.path()).self_improvement, defaults);
    let saved = std::fs::read_to_string(&path).expect("read");
    assert!(saved.contains("version = 2"));
    assert!(!saved.contains("draft_dir"));
}

#[test]
fn invalid_input_never_changes_existing_file_or_creates_a_new_one() {
    let directory = tempfile::tempdir().expect("temp");
    let path = directory.path().join("evorch.toml");
    let missing = directory.path().join("new.toml");
    let original = "version = 2\n# unchanged\n[metrics]\nretention_days = 7\n";
    std::fs::write(&path, original).expect("write");
    for invalid in [
        SelfImprovementConfig {
            daily_limit: 0,
            ..Default::default()
        },
        SelfImprovementConfig {
            max_candidates: 10_001,
            ..Default::default()
        },
        SelfImprovementConfig {
            evidence_max_bytes: 255,
            ..Default::default()
        },
        SelfImprovementConfig {
            duplicate_cooldown_secs: u64::MAX,
            ..Default::default()
        },
    ] {
        for target in [&path, &missing] {
            assert!(matches!(
                save_self_improvement(target, &invalid),
                Err(ConfigError::InvalidField { .. })
            ));
            assert!(!target.with_extension("toml.tmp").exists());
        }
        assert_eq!(std::fs::read_to_string(&path).expect("read"), original);
        assert!(!missing.exists());
    }
}

#[test]
fn save_strictly_revalidates_unrelated_sections_before_writing() {
    let directory = tempfile::tempdir().expect("temp");
    let path = directory.path().join("evorch.toml");
    let original = "version = 2\n[metrics]\nretention_dayz = 7\n";
    std::fs::write(&path, original).expect("write");

    let error = save_self_improvement(&path, &SelfImprovementConfig::default())
        .expect_err("unrelated typo is invalid");

    assert!(
        matches!(error, ConfigError::InvalidField { path, .. } if path == "metrics.retention_dayz")
    );
    assert_eq!(std::fs::read_to_string(&path).expect("read"), original);
    assert!(!path.with_extension("toml.tmp").exists());
}

#[test]
fn existing_save_api_also_rejects_invalid_self_improvement_values() {
    let directory = tempfile::tempdir().expect("temp");
    let path = directory.path().join("evorch.toml");
    let original = "version = 2\n[self_improvement]\ndaily_limit = 0\n";
    std::fs::write(&path, original).expect("write");

    let error = save_sandbox(&path, SandboxConfig::default()).expect_err("invalid range");

    assert!(
        matches!(error, ConfigError::InvalidField { path, .. } if path == "self_improvement.daily_limit")
    );
    assert_eq!(std::fs::read_to_string(&path).expect("read"), original);
    assert!(!path.with_extension("toml.tmp").exists());
}

#[test]
fn save_does_not_overwrite_existing_temporary_file() {
    let directory = tempfile::tempdir().expect("temp");
    let path = directory.path().join("evorch.toml");
    let temporary_path = path.with_extension("toml.tmp");
    let original = "version = 2\n";
    std::fs::write(&path, original).expect("write config");
    std::fs::write(&temporary_path, "keep temporary file").expect("write temporary file");

    assert!(save_self_improvement(&path, &SelfImprovementConfig::default()).is_err());

    assert_eq!(std::fs::read_to_string(&path).expect("read"), original);
    assert_eq!(
        std::fs::read_to_string(&temporary_path).expect("read temporary file"),
        "keep temporary file"
    );
}
