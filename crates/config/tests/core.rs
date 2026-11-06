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
