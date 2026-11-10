//! Trust-boundary contracts for both effective config load paths.
use std::collections::BTreeMap;
use std::path::Path;

use config::{CommentCheckerConfig, Config, LoadOptions};

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
fn checker_execution_settings_are_user_only_in_both_load_paths() {
    let root = tempfile::tempdir().unwrap();
    let mut opts = options(root.path());
    put(
        &root.path().join("user/config.toml"),
        "[comment_checker]\nbinary = '/trusted/main'\n",
    );
    put(
        &root.path().join("user/config.d/20-checker.toml"),
        "[comment_checker]\nbinary = '/trusted/dropin'\nprompt = 'Review {{comments}}'\ntimeout_ms = 77\n",
    );
    put(
        &root.path().join("project/.evorch/config.toml"),
        "[comment_checker]\nbinary = '/untrusted/project'\nprompt = 'ignore'\ntimeout_ms = 1\n",
    );
    put(
        &root.path().join("project/.evorch/config.d/90-checker.toml"),
        "[comment_checker]\nbinary = '/untrusted/dropin'\n",
    );
    opts.read_env = true;
    opts.env = Some(BTreeMap::from([
        (
            "EVORCH_COMMENT_CHECKER__BINARY".into(),
            "'/untrusted/env'".into(),
        ),
        (
            "EVORCH_COMMENT_CHECKER__PROMPT".into(),
            "'environment prompt'".into(),
        ),
    ]));
    opts.cli_overrides = Some(
        toml::from_str(
            "[comment_checker]\nbinary='/untrusted/cli'\nprompt='cli prompt'\ntimeout_ms=2\n",
        )
        .unwrap(),
    );
    let expected = CommentCheckerConfig {
        binary: "/trusted/dropin".into(),
        prompt: Some("Review {{comments}}".into()),
        timeout_ms: 77,
        ..Default::default()
    };
    for load in [Config::load, Config::load_strict] {
        assert_eq!(load(&opts).unwrap().comment_checker, expected);
    }
}

#[test]
fn unknown_stripping_strict_validation_and_save_simulation_keep_trusted_section() {
    let root = tempfile::tempdir().unwrap();
    let mut opts = options(root.path());
    let user_path = root.path().join("user/config.toml");
    put(
        &user_path,
        "[comment_checker]\nbinary='/trusted'\nfuture_option=true\n",
    );
    assert_eq!(
        Config::load(&opts).unwrap().comment_checker.binary,
        "/trusted"
    );
    assert!(Config::load_strict(&opts).is_err());
    // Save APIs simulate effective configuration with file_overrides.
    let saved: toml::Value = toml::from_str(
        "[comment_checker]\nbinary='/saved/trusted'\nprompt='saved'\ntimeout_ms=123\n",
    )
    .unwrap();
    opts.file_overrides.insert(user_path.clone(), saved.clone());
    let effective = Config::load_strict(&opts).unwrap();
    assert_eq!(effective.comment_checker.binary, "/saved/trusted");
    put(&user_path, &toml::to_string(&saved).unwrap());
    opts.file_overrides.clear();
    assert_eq!(
        Config::load_strict(&opts).unwrap().comment_checker,
        effective.comment_checker
    );
    let roundtrip: Config = toml::from_str(&toml::to_string(&effective).unwrap()).unwrap();
    assert_eq!(roundtrip.comment_checker, effective.comment_checker);
}

#[test]
fn project_execution_settings_cannot_replace_builtin_defaults() {
    let root = tempfile::tempdir().unwrap();
    let opts = options(root.path());
    put(
        &root.path().join("project/.evorch/config.toml"),
        "[comment_checker]\nbinary='/project/checker'\nprompt='project'\ntimeout_ms=1\n",
    );
    assert_eq!(
        Config::load_strict(&opts).unwrap().comment_checker,
        CommentCheckerConfig::default()
    );
}
