//! `[pull_requests]` is a trusted section (ADR 0043): only defaults and user/global
//! files can enable it, and the settings are validated on every load path.
use std::collections::BTreeMap;
use std::path::Path;

use config::{Config, ConfigError, LoadOptions, PullRequestsConfig};

fn put(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn options(root: &Path) -> LoadOptions {
    LoadOptions {
        user_config_dir: Some(root.join("user")),
        project_dir: Some(root.join("project")),
        read_env: false,
        ..Default::default()
    }
}

#[test]
fn tracking_is_disabled_by_default_in_both_load_paths() {
    let root = tempfile::tempdir().unwrap();
    let opts = options(root.path());
    for load in [Config::load, Config::load_strict] {
        let loaded = load(&opts).unwrap().pull_requests;
        assert_eq!(loaded, PullRequestsConfig::default());
        assert!(!loaded.enabled);
        assert!(loaded.include_in_review);
    }
}

#[test]
fn a_project_layer_cannot_enable_tracking_but_a_user_layer_can() {
    let root = tempfile::tempdir().unwrap();
    let opts = options(root.path());
    put(
        &root.path().join("project/.evorch/config.toml"),
        "[pull_requests]\nenabled = true\npending_poll_seconds = 6\n",
    );
    put(
        &root.path().join("project/.evorch/config.d/90-pr.toml"),
        "[pull_requests]\nenabled = true\n",
    );
    for load in [Config::load, Config::load_strict] {
        assert_eq!(
            load(&opts).unwrap().pull_requests,
            PullRequestsConfig::default(),
            "project files never reach this section"
        );
    }

    put(
        &root.path().join("user/config.toml"),
        "[pull_requests]\nenabled = true\n",
    );
    put(
        &root.path().join("user/config.d/20-pr.toml"),
        "[pull_requests]\npending_poll_seconds = 12\ninclude_in_review = false\n",
    );
    let expected = PullRequestsConfig {
        enabled: true,
        include_in_review: false,
        pending_poll_seconds: 12,
        ..Default::default()
    };
    for load in [Config::load, Config::load_strict] {
        assert_eq!(
            load(&opts).unwrap().pull_requests,
            expected,
            "the user layer still applies when a project layer also asks"
        );
    }
}

#[test]
fn environment_and_cli_overrides_cannot_change_the_section() {
    let root = tempfile::tempdir().unwrap();
    let mut opts = options(root.path());
    put(
        &root.path().join("user/config.toml"),
        "[pull_requests]\nstable_poll_seconds = 600\n",
    );
    opts.read_env = true;
    opts.env = Some(BTreeMap::from([(
        "EVORCH_PULL_REQUESTS__ENABLED".into(),
        "true".into(),
    )]));
    opts.cli_overrides = Some(toml::from_str("[pull_requests]\nenabled = true\n").unwrap());
    for load in [Config::load, Config::load_strict] {
        let loaded = load(&opts).unwrap().pull_requests;
        assert!(!loaded.enabled);
        assert_eq!(loaded.stable_poll_seconds, 600);
    }
}

#[test]
fn invalid_user_values_and_unknown_keys_are_rejected() {
    let root = tempfile::tempdir().unwrap();
    let opts = options(root.path());
    let user = root.path().join("user/config.toml");

    put(&user, "[pull_requests]\npending_poll_seconds = 1\n");
    for load in [Config::load, Config::load_strict] {
        match load(&opts) {
            Err(ConfigError::InvalidField { path, .. }) => {
                assert_eq!(path, "pull_requests.pending_poll_seconds");
            }
            other => panic!("expected a range error, got {other:?}"),
        }
    }

    put(
        &user,
        "[pull_requests]\nenabled = true\nfuture_option = 1\n",
    );
    assert!(
        Config::load(&opts).unwrap().pull_requests.enabled,
        "a tolerant load ignores unknown keys"
    );
    assert!(
        Config::load_strict(&opts).is_err(),
        "a strict load rejects them"
    );
}
