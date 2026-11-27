//! The Features sections are edited in place: only the keys of the edit change, and a
//! key that is absent and already effective at the wanted value is not created.

use std::num::NonZeroU32;
use std::path::PathBuf;

use config::{
    BrowserToolsConfig, Config, FeatureSettingsEdit, LoadOptions, PullRequestsConfig,
    ThreadRetentionConfig, edit_feature_settings_text,
};

fn effective(text: &str) -> Config {
    Config::load_unresolved_with_file_text(
        &LoadOptions {
            user_config_dir: Some(PathBuf::from("/nonexistent/evorch-feature-edit")),
            read_env: false,
            ..Default::default()
        },
        &PathBuf::from("/nonexistent/evorch-feature-edit/config.toml"),
        text,
    )
    .unwrap()
}

fn full_edit() -> FeatureSettingsEdit {
    FeatureSettingsEdit {
        pull_requests: Some(PullRequestsConfig {
            enabled: true,
            auto_fix_enabled: false,
            pending_poll_seconds: 45,
            stable_poll_seconds: 600,
            fetch_timeout_seconds: 30,
            ..Default::default()
        }),
        web_tools_enabled: Some(false),
        browser: Some(BrowserToolsConfig {
            enabled: false,
            executable: Some(PathBuf::from("/opt/chromium")),
            max_pages: NonZeroU32::new(6).unwrap(),
            max_tabs: NonZeroU32::new(2).unwrap(),
            coordinate_actions: true,
        }),
        thread_retention: Some(ThreadRetentionConfig {
            auto_archive_after_days: 0,
            archived_delete_after_days: 30,
        }),
    }
}

#[test]
fn all_four_sections_are_written_and_every_other_key_and_comment_stays() {
    // Given: a file with unrelated keys, comments and one of the keys to change.
    let text = "version = 3\n# keep me\n[core]\nidle_grace_secs = 7 # grace\n\n[sandbox]\n# web before\nweb_tools_enabled = true # web after\nescalation_approval = \"user\"\n\n[tools.run_js]\nenabled = true\n";
    let baseline = effective(text);

    // When: the four sections are edited.
    let edited = edit_feature_settings_text(text, &full_edit(), &baseline).unwrap();

    // Then: they are effective, and the neighbouring settings are untouched.
    let config = effective(&edited);
    assert_eq!(
        full_edit().differing(&config),
        Vec::<&str>::new(),
        "{edited}"
    );
    assert_eq!(config.core.idle_grace_secs, 7);
    assert!(config.tools.run_js.enabled);
    assert_eq!(
        config.sandbox.escalation_approval,
        baseline.sandbox.escalation_approval
    );
    assert!(edited.contains("# keep me\n"), "{edited}");
    assert!(edited.contains("idle_grace_secs = 7 # grace\n"), "{edited}");
    assert!(
        edited.contains("# web before\nweb_tools_enabled = false # web after\n"),
        "{edited}"
    );
    assert!(
        edited.contains("[tools.run_js]\nenabled = true"),
        "{edited}"
    );
}

#[test]
fn a_value_that_is_already_effective_and_absent_is_not_written() {
    // Given: an empty file and an edit that only changes the web tools and one PR key.
    let baseline = Config::default();
    let edit = FeatureSettingsEdit {
        pull_requests: Some(PullRequestsConfig {
            enabled: true,
            ..baseline.pull_requests
        }),
        web_tools_enabled: Some(true),
        browser: Some(baseline.tools.browser.clone()),
        thread_retention: Some(baseline.storage.thread_retention),
    };

    // When: it is applied.
    let edited = edit_feature_settings_text("", &edit, &baseline).unwrap();

    // Then: only the changed key is created.
    assert!(
        edited.contains("[pull_requests]\nenabled = true"),
        "{edited}"
    );
    for untouched in ["sandbox", "tools", "storage", "poll"] {
        assert!(!edited.contains(untouched), "{untouched} in {edited}");
    }
}

#[test]
fn an_unset_executable_is_removed_and_the_other_browser_keys_stay() {
    // Given: a browser section with an executable and a comment.
    let text =
        "version = 3\n[tools.browser]\n# my chrome\nexecutable = \"/opt/chrome\"\nmax_tabs = 3\n";
    let baseline = effective(text);
    let edit = FeatureSettingsEdit {
        browser: Some(BrowserToolsConfig {
            executable: None,
            ..baseline.tools.browser.clone()
        }),
        ..Default::default()
    };

    // When: the executable is unset.
    let edited = edit_feature_settings_text(text, &edit, &baseline).unwrap();

    // Then: the key is gone and the rest of the section is as it was.
    assert!(!edited.contains("executable"), "{edited}");
    assert!(edited.contains("max_tabs = 3"), "{edited}");
    assert_eq!(effective(&edited).tools.browser.executable, None);
}

#[test]
fn a_text_that_is_not_toml_is_refused() {
    let error = edit_feature_settings_text("[pull_requests\n", &full_edit(), &Config::default())
        .unwrap_err();
    assert!(error.to_string().contains("TOML"), "{error}");
}
