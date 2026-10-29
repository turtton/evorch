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

fn user_only(text: &str) -> (tempfile::TempDir, LoadOptions) {
    let root = tempfile::tempdir().unwrap();
    put(&root.path().join("user/config.toml"), text);
    let opts = LoadOptions {
        user_config_dir: Some(root.path().join("user")),
        read_env: false,
        ..Default::default()
    };
    (root, opts)
}

#[test]
fn run_js_scopes_load_from_the_user_layer_and_ignore_the_project_layer() {
    // Given: user scopes and a project layer that tries to widen them.
    let root = tempfile::tempdir().unwrap();
    put(
        &root.path().join("user/config.toml"),
        "[tools.run_js]\nenabled = true\nscopes = [\"worker:quick\", \"reviewer\"]\n",
    );
    put(
        &root.path().join("project/.evorch/config.toml"),
        "[tools.run_js]\nscopes = [\"worker\", \"explorer\"]\n",
    );
    let opts = LoadOptions {
        user_config_dir: Some(root.path().join("user")),
        project_dir: Some(root.path().join("project")),
        read_env: false,
        ..Default::default()
    };

    // When / Then: both load paths keep the user's scopes only.
    for load in [Config::load, Config::load_strict] {
        assert_eq!(
            load(&opts).unwrap().tools.run_js.scopes,
            ["worker:quick", "reviewer"]
        );
    }
}

#[test]
fn invalid_run_js_scopes_are_rejected_with_their_path() {
    for (scopes, path, reason) in [
        ("[\"wroker\"]", "tools.run_js.scopes[0]", "unknown role"),
        (
            "[\"worker:deep\", \"worker:nope\"]",
            "tools.run_js.scopes[1]",
            "unknown category `nope`",
        ),
        ("[\"\"]", "tools.run_js.scopes[0]", "unknown role"),
        (
            "[\"explorer:deep\"]",
            "tools.run_js.scopes[0]",
            "no categories",
        ),
    ] {
        // Given: a user file with an invalid scope.
        let (_root, opts) = user_only(&format!("[tools.run_js]\nscopes = {scopes}\n"));

        // When / Then: strict loading names the entry and the reason.
        let error = Config::load_strict(&opts).unwrap_err().to_string();
        assert!(error.contains(path) && error.contains(reason), "{error}");
    }
}

#[test]
fn tolerant_load_drops_unknown_run_js_scopes_but_not_malformed_ones() {
    // Given: scopes naming a category and a role this build does not know.
    let (_root, opts) =
        user_only("[tools.run_js]\nscopes = [\"worker:deep\", \"worker:retired\", \"gone\"]\n");

    // When / Then: strict refuses; tolerant keeps the known entry.
    assert!(Config::load_strict(&opts).is_err());
    assert_eq!(
        Config::load(&opts).unwrap().tools.run_js.scopes,
        ["worker:deep"]
    );

    // Given: entries that are not scope strings at all.
    for scopes in ["[\"\"]", "[1]", "\"worker\""] {
        let (_root, opts) = user_only(&format!("[tools.run_js]\nscopes = {scopes}\n"));
        // Then: even the tolerant load refuses them.
        assert!(Config::load(&opts).is_err(), "{scopes}");
    }
}
