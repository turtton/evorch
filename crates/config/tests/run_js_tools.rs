//! `[tools.run_js]` widens what agents can do (ADR 0039), so only the user
//! layer may set it, and strict loading names unknown keys.
use std::path::Path;

use config::{Config, LoadOptions, RunJsConfig};

fn put(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

#[test]
fn run_js_settings_come_only_from_the_user_layer() {
    // Given: user settings that enable run_js and a project layer that disagrees.
    let root = tempfile::tempdir().unwrap();
    put(
        &root.path().join("user/config.toml"),
        "[tools.run_js]\nenabled = true\nmax_tool_calls = 50\n",
    );
    put(
        &root.path().join("project/.evorch/config.toml"),
        "[tools.run_js]\nmax_tool_calls = 5000\nmemory_limit_mib = 4096\n",
    );
    let opts = LoadOptions {
        user_config_dir: Some(root.path().join("user")),
        project_dir: Some(root.path().join("project")),
        read_env: false,
        ..Default::default()
    };

    // When / Then: both load paths keep the user's values only.
    let expected = RunJsConfig {
        enabled: true,
        max_tool_calls: 50,
        ..Default::default()
    };
    for load in [Config::load, Config::load_strict] {
        assert_eq!(load(&opts).unwrap().tools.run_js, expected);
    }
}

#[test]
fn unknown_run_js_keys_are_rejected_by_strict_load() {
    let root = tempfile::tempdir().unwrap();
    put(
        &root.path().join("user/config.toml"),
        "[tools.run_js]\ntimeout = 1\n",
    );
    let opts = LoadOptions {
        user_config_dir: Some(root.path().join("user")),
        read_env: false,
        ..Default::default()
    };

    let error = Config::load_strict(&opts).unwrap_err().to_string();

    assert!(error.contains("tools.run_js.timeout"), "{error}");
    assert_eq!(
        Config::load(&opts).unwrap().tools.run_js,
        RunJsConfig::default()
    );
}
