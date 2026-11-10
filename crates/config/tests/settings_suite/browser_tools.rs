//! `[tools.browser]` launches a program and widens agent capabilities, so only
//! the user layer may set it.
use std::path::Path;

use config::{BrowserToolsConfig, Config, LoadOptions};

fn put(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

#[test]
fn browser_tool_settings_come_only_from_the_user_layer() {
    // Given: user settings and conflicting project, env and CLI layers.
    let root = tempfile::tempdir().unwrap();
    put(
        &root.path().join("user/config.toml"),
        "[tools.browser]\nexecutable = '/trusted/chromium'\nmax_pages = 3\nmax_tabs = 2\n",
    );
    put(
        &root.path().join("project/.evorch/config.toml"),
        "[tools.browser]\nexecutable = '/untrusted/chromium'\ncoordinate_actions = true\nenabled = false\nmax_tabs = 6\n",
    );
    let opts = LoadOptions {
        user_config_dir: Some(root.path().join("user")),
        project_dir: Some(root.path().join("project")),
        read_env: true,
        env: Some(
            [(
                "EVORCH_TOOLS__BROWSER__COORDINATE_ACTIONS".to_owned(),
                "true".to_owned(),
            )]
            .into(),
        ),
        cli_overrides: Some(toml::from_str("[tools.browser]\nmax_pages = 1\n").unwrap()),
        ..Default::default()
    };

    // When / Then: both load paths keep the user's values only.
    let expected = BrowserToolsConfig {
        executable: Some("/trusted/chromium".into()),
        max_pages: 3.try_into().unwrap(),
        max_tabs: 2.try_into().unwrap(),
        ..Default::default()
    };
    for load in [Config::load, Config::load_strict] {
        assert_eq!(load(&opts).unwrap().tools.browser, expected);
    }
}

#[test]
fn unknown_browser_tool_keys_are_rejected_by_strict_load() {
    let root = tempfile::tempdir().unwrap();
    put(
        &root.path().join("user/config.toml"),
        "[tools.browser]\ncoordinates = true\n",
    );
    let opts = LoadOptions {
        user_config_dir: Some(root.path().join("user")),
        read_env: false,
        ..Default::default()
    };

    let error = Config::load_strict(&opts).unwrap_err().to_string();

    assert!(error.contains("tools.browser.coordinates"), "{error}");
    assert_eq!(
        Config::load(&opts).unwrap().tools.browser,
        BrowserToolsConfig::default()
    );
}
