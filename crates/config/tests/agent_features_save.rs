//! The agent feature switches are written only where they apply, and saving
//! them keeps every neighbouring setting.

use std::path::Path;

use config::{AgentFeatures, Config, LayeredSaveContext, LoadOptions, RunJsConfig};

fn put(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn user_options(root: &Path) -> LoadOptions {
    LoadOptions {
        user_config_dir: Some(root.join("user")),
        read_env: false,
        ..Default::default()
    }
}

#[test]
fn saving_agent_features_keeps_other_run_js_and_sandbox_keys() {
    // Given: a user file with custom run_js limits, sandbox approval, and comments.
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("user/config.toml");
    put(
        &path,
        "version = 3\n# keep me\n[sandbox]\nescalation_approval = \"user\"\n\n[tools.run_js]\nmax_tool_calls = 50\nmemory_limit_mib = 128\n",
    );
    let options = user_options(root.path());

    // When: enabling run_js and disabling Web tools.
    LayeredSaveContext::new(&options)
        .save_agent_features(
            &path,
            &AgentFeatures {
                web_tools_enabled: false,
                run_js_enabled: true,
                ..AgentFeatures::default()
            },
        )
        .unwrap();

    // Then: both switches load, and every other key and comment survives.
    let loaded = Config::load_strict(&options).unwrap();
    assert_eq!(
        loaded.tools.run_js,
        RunJsConfig {
            enabled: true,
            max_tool_calls: 50,
            memory_limit_mib: 128,
            ..Default::default()
        }
    );
    assert!(!loaded.sandbox.web_tools_enabled);
    assert_eq!(
        loaded.sandbox.escalation_approval,
        config::EscalationApproval::User
    );
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("# keep me"), "{text}");

    // When: switching both back.
    LayeredSaveContext::new(&options)
        .save_agent_features(&path, &AgentFeatures::default())
        .unwrap();

    // Then: the defaults load while the limits stay.
    let loaded = Config::load_strict(&options).unwrap();
    assert_eq!(
        AgentFeatures::from_config(&loaded),
        AgentFeatures::default()
    );
    assert_eq!(loaded.tools.run_js.max_tool_calls, 50);
}

#[test]
fn saving_agent_features_creates_missing_sections() {
    // Given: no user file yet.
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("user/config.toml");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let options = user_options(root.path());
    let features = AgentFeatures {
        web_tools_enabled: true,
        run_js_enabled: true,
        ..AgentFeatures::default()
    };

    // When: saving the switches.
    LayeredSaveContext::new(&options)
        .save_agent_features(&path, &features)
        .unwrap();

    // Then: they reload with run_js limits left at their defaults.
    let loaded = Config::load_strict(&options).unwrap();
    assert_eq!(AgentFeatures::from_config(&loaded), features);
    assert_eq!(
        loaded.tools.run_js,
        RunJsConfig {
            enabled: true,
            ..Default::default()
        }
    );
}

#[test]
fn run_js_switch_is_refused_where_it_would_not_apply() {
    let features = AgentFeatures {
        web_tools_enabled: true,
        run_js_enabled: true,
        ..AgentFeatures::default()
    };
    // Given: a project file, which may not set [tools], and a user drop-in that
    // overrides the main user file.
    let root = tempfile::tempdir().unwrap();
    let project_path = config::project_main_config_path(&root.path().join("project"));
    put(&project_path, "version = 3\n");
    let user_path = root.path().join("user/config.toml");
    put(&user_path, "version = 3\n");
    put(
        &root.path().join("user/config.d/tools.toml"),
        "[tools.run_js]\nenabled = false\n",
    );
    let options = LoadOptions {
        project_dir: Some(root.path().join("project")),
        ..user_options(root.path())
    };

    for path in [&project_path, &user_path] {
        let before = std::fs::read_to_string(path).unwrap();

        // When: saving run_js on into that file.
        let error = LayeredSaveContext::new(&options)
            .save_agent_features(path, &features)
            .unwrap_err();

        // Then: the save names the setting and leaves the file unchanged.
        assert!(
            error.to_string().contains("tools.run_js.enabled"),
            "{error}"
        );
        assert_eq!(std::fs::read_to_string(path).unwrap(), before);
    }
}

#[test]
fn saving_agent_features_keeps_the_comments_on_both_switches() {
    // Given: both switches with a comment line before and a comment after them.
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("user/config.toml");
    put(
        &path,
        "version = 3\n[sandbox]\n# web before\nweb_tools_enabled = true # web after\n\n[tools.run_js]\n# run_js before\nenabled = false # run_js after\nmax_tool_calls = 50\n",
    );
    let options = user_options(root.path());
    let flipped = AgentFeatures {
        web_tools_enabled: false,
        run_js_enabled: true,
        ..AgentFeatures::default()
    };

    // When / Then: changing both values, and then saving them unchanged, keeps
    // every comment next to the updated values.
    for _ in 0..2 {
        LayeredSaveContext::new(&options)
            .save_agent_features(&path, &flipped)
            .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("# web before\nweb_tools_enabled = false # web after\n"),
            "{text}"
        );
        assert!(
            text.contains("# run_js before\nenabled = true # run_js after\n"),
            "{text}"
        );
        assert_eq!(
            AgentFeatures::from_config(&Config::load_strict(&options).unwrap()),
            flipped
        );
    }
}

#[test]
fn saving_run_js_scopes_rewrites_the_list_in_place_and_keeps_the_comments() {
    // Given: a user file with commented scopes and neighbouring keys.
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("user/config.toml");
    put(
        &path,
        "version = 3\n[tools.run_js]\nenabled = true\n# scopes before\nscopes = [\"worker:deep\"] # scopes after\nmax_tool_calls = 50\n",
    );
    let options = user_options(root.path());
    let features = AgentFeatures {
        run_js_enabled: true,
        run_js_scopes: vec!["worker".into(), "reviewer:plan-review".into()],
        ..AgentFeatures::default()
    };

    // When: saving other scopes.
    LayeredSaveContext::new(&options)
        .save_agent_features(&path, &features)
        .unwrap();

    // Then: they load, and the comments and the other key stay.
    let loaded = Config::load_strict(&options).unwrap();
    assert_eq!(loaded.tools.run_js.scopes, features.run_js_scopes);
    assert_eq!(loaded.tools.run_js.max_tool_calls, 50);
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("# scopes before\nscopes = ["), "{text}");
    assert!(text.contains("] # scopes after\n"), "{text}");

    // When: saving the same scopes again.
    let before = std::fs::read_to_string(&path).unwrap();
    LayeredSaveContext::new(&options)
        .save_agent_features(&path, &features)
        .unwrap();

    // Then: the file is byte-identical.
    assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
}

#[test]
fn run_js_scopes_are_created_only_when_they_differ_from_the_default() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("user/config.toml");
    put(&path, "version = 3\n");
    let options = user_options(root.path());

    // When: saving the default scopes.
    LayeredSaveContext::new(&options)
        .save_agent_features(&path, &AgentFeatures::default())
        .unwrap();
    // Then: no scopes key is written.
    assert!(!std::fs::read_to_string(&path).unwrap().contains("scopes"));

    // When: saving other scopes, including an empty list.
    for scopes in [vec!["explorer".to_owned()], Vec::new()] {
        let features = AgentFeatures {
            run_js_scopes: scopes.clone(),
            ..AgentFeatures::default()
        };
        LayeredSaveContext::new(&options)
            .save_agent_features(&path, &features)
            .unwrap();
        // Then: they reload as saved.
        assert_eq!(
            Config::load_strict(&options).unwrap().tools.run_js.scopes,
            scopes
        );
    }
}

#[test]
fn run_js_scopes_that_are_invalid_or_overridden_are_refused_without_writing() {
    // Given: a user file and a drop-in that pins different scopes.
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("user/config.toml");
    put(&path, "version = 3\n");
    put(
        &root.path().join("user/config.d/tools.toml"),
        "[tools.run_js]\nscopes = [\"advisor\"]\n",
    );
    let options = user_options(root.path());
    let before = std::fs::read_to_string(&path).unwrap();

    for scopes in [vec!["worker".to_owned()], vec!["wroker".to_owned()]] {
        // When: saving scopes the drop-in overrides, or that name no role.
        let error = LayeredSaveContext::new(&options)
            .save_agent_features(
                &path,
                &AgentFeatures {
                    run_js_scopes: scopes,
                    ..AgentFeatures::default()
                },
            )
            .unwrap_err();

        // Then: the save names the setting and leaves the file unchanged.
        assert!(error.to_string().contains("tools.run_js.scopes"), "{error}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    }
}
