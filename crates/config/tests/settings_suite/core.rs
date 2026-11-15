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
fn learning_drain_timeout_is_a_bounded_user_setting_with_a_path_in_every_error() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("user")).unwrap();
    let opts = LoadOptions {
        user_config_dir: Some(dir.path().join("user")),
        project_dir: Some(dir.path().join("project")),
        read_env: false,
        ..Default::default()
    };
    let default = Config::load_strict(&opts).unwrap().core.restart;
    assert!(
        config::types::LEARNING_DRAIN_TIMEOUT_SECS.contains(&default.learning_drain_timeout_secs)
    );
    let path = dir.path().join("user/config.toml");
    for accepted in [1_u64, 3600] {
        std::fs::write(
            &path,
            format!("[core.restart]\nlearning_drain_timeout_secs = {accepted}\n"),
        )
        .unwrap();
        let configured = Config::load_strict(&opts).unwrap().core.restart;
        assert_eq!(configured.learning_drain_timeout_secs, accepted);
        assert_eq!(
            configured.learning_drain_timeout(),
            std::time::Duration::from_secs(accepted)
        );
    }
    for rejected in ["0", "3601", "-1"] {
        std::fs::write(
            &path,
            format!("[core.restart]\nlearning_drain_timeout_secs = {rejected}\n"),
        )
        .unwrap();
        assert!(
            Config::load_strict(&opts).is_err(),
            "{rejected} must be rejected"
        );
    }
    std::fs::write(&path, "[core.restart]\nlearning_drain_timeout_secs = 0\n").unwrap();
    assert!(
        Config::load_strict(&opts)
            .unwrap_err()
            .to_string()
            .contains("core.restart.learning_drain_timeout_secs")
    );
    std::fs::write(&path, "[core.restart]\nlearning_drain_timeout = 5\n").unwrap();
    assert!(
        Config::load_strict(&opts)
            .unwrap_err()
            .to_string()
            .contains("core.restart.learning_drain_timeout")
    );
    // The project layer cannot lower the limit that the user chose.
    std::fs::write(&path, "[core.restart]\nlearning_drain_timeout_secs = 90\n").unwrap();
    std::fs::create_dir_all(dir.path().join("project/.evorch")).unwrap();
    std::fs::write(
        dir.path().join("project/.evorch/config.toml"),
        "[core.restart]\nlearning_drain_timeout_secs = 1\n",
    )
    .unwrap();
    assert_eq!(
        Config::load_strict(&opts)
            .unwrap()
            .core
            .restart
            .learning_drain_timeout_secs,
        90
    );
}
