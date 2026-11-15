use config::{Config, LoadOptions};

#[test]
fn core_and_web_are_opt_in_trusted_settings_and_strictly_typed() {
    let dir = tempfile::tempdir().unwrap();
    let opts = LoadOptions {
        user_config_dir: Some(dir.path().join("user")),
        project_dir: Some(dir.path().join("project")),
        read_env: false,
        ..Default::default()
    };
    let defaults = Config::load_strict(&opts).unwrap();
    assert!(!defaults.web.enabled);
    assert_eq!(defaults.web.port, 7878);
    assert_eq!(defaults.core.idle_grace_secs, 60);
    std::fs::create_dir_all(dir.path().join("user")).unwrap();
    std::fs::create_dir_all(dir.path().join("project/.evorch")).unwrap();
    std::fs::write(
        dir.path().join("project/.evorch/config.toml"),
        "[web]\nenabled = true\n",
    )
    .unwrap();
    assert!(!Config::load_strict(&opts).unwrap().web.enabled);
    let path = dir.path().join("user/config.toml");
    std::fs::write(&path, "[web]\nenabled = true\nport = 8080\nallowed_hosts = ['core.tailnet.ts.net']\n[core]\nidle_grace_secs = 120\n").unwrap();
    let configured = Config::load_strict(&opts).unwrap();
    assert!(configured.web.enabled);
    assert_eq!(configured.web.allowed_hosts, ["core.tailnet.ts.net"]);
    assert_eq!(configured.core.idle_grace_secs, 120);
    std::fs::write(&path, "[web]\nenabeld = true\n").unwrap();
    assert!(
        Config::load_strict(&opts)
            .unwrap_err()
            .to_string()
            .contains("web.enabeld")
    );
    std::fs::write(&path, "[web]\nport = 65536\n").unwrap();
    assert!(Config::load_strict(&opts).is_err());
}

#[test]
fn restart_allowlist_is_local_file_only_and_reloaded() {
    let dir = tempfile::tempdir().unwrap();
    let user = dir.path().join("user");
    let project = dir.path().join("project");
    std::fs::create_dir_all(&user).unwrap();
    std::fs::create_dir_all(project.join(".evorch")).unwrap();
    std::fs::write(
        user.join("config.toml"),
        "[core.restart.targets.next]\npath='/local/core'\n",
    )
    .unwrap();
    std::fs::write(
        project.join(".evorch/config.toml"),
        "[core.restart.targets.next]\npath='/project/injected'\n",
    )
    .unwrap();
    let options = LoadOptions {
        user_config_dir: Some(user.clone()), project_dir: Some(project), read_env: false,
        cli_overrides: Some(toml::from_str("[core.restart.targets.next]\npath='/cli/injected'\n[core.restart.targets.evil]\npath='/evil'").unwrap()),
        ..Default::default()
    };
    let first = Config::load_strict(&options).unwrap().core.restart;
    assert_eq!(first.timeout_secs, 300);
    assert_eq!(first.targets.len(), 1);
    assert_eq!(
        first.targets["next"].path,
        std::path::Path::new("/local/core")
    );
    std::fs::write(
        user.join("config.toml"),
        "[core.restart]\ntimeout_secs=21\n[core.restart.targets.next]\npath='/updated/core'\n",
    )
    .unwrap();
    let second = Config::load_restart_settings(&user).unwrap();
    assert_eq!(second.target_path("current").unwrap(), None);
    assert_eq!(
        second.target_path("next").unwrap().unwrap(),
        std::path::Path::new("/updated/core")
    );
    assert!(second.target_path("missing").is_err());
    assert_eq!(first.timeout_secs, 300);
    assert_eq!(second.timeout_secs, 21);
    assert_ne!(first.targets, second.targets);
}
#[test]
fn restart_config_enforces_bounds_and_reserved_current() {
    use config::{RestartConfig, RestartTargetConfig, validate_restart_target_id};
    for id in ["", "../next", "next.path", "日本語", &"a".repeat(65)] {
        assert!(validate_restart_target_id(id).is_err());
    }
    validate_restart_target_id(&"a".repeat(64)).unwrap();
    let mut config = RestartConfig::default();
    for seconds in [0, 3601] {
        config.timeout_secs = seconds;
        assert!(config.validate().is_err());
    }
    for seconds in [1, 3600] {
        config.timeout_secs = seconds;
        config.validate().unwrap();
    }
    for path in ["relative", "/has\0nul", &format!("/{}", "a".repeat(4096))] {
        config
            .targets
            .insert("next".into(), RestartTargetConfig { path: path.into() });
        assert!(config.validate().is_err());
    }
    config.targets.clear();
    config.targets.insert(
        "current".into(),
        RestartTargetConfig {
            path: "/absolute".into(),
        },
    );
    assert!(config.validate().is_err());
    config.targets.clear();
    for n in 0..32 {
        config.targets.insert(
            format!("t{n}"),
            RestartTargetConfig {
                path: "/absolute".into(),
            },
        );
    }
    config.validate().unwrap();
    config.targets.insert(
        "extra".into(),
        RestartTargetConfig {
            path: "/absolute".into(),
        },
    );
    assert!(config.validate().is_err());
}
#[test]
fn settings_save_guard_rejects_add_delete_change_but_preserves_omitted_patch() {
    use config::{RestartTargetConfig, reject_restart_target_changes};
    let mut current = Config::default();
    current.core.restart.targets.insert(
        "next".into(),
        RestartTargetConfig {
            path: "/original".into(),
        },
    );
    let mut patch_candidate = current.clone();
    patch_candidate.core.idle_grace_secs += 1;
    reject_restart_target_changes(&current, &patch_candidate).unwrap();
    assert!(reject_restart_target_changes(&current, &Config::default()).is_err());
    for id in ["next", "extra"] {
        let mut candidate = current.clone();
        candidate.core.restart.targets.insert(
            id.into(),
            RestartTargetConfig {
                path: "/changed".into(),
            },
        );
        assert!(reject_restart_target_changes(&current, &candidate).is_err());
    }
}
