use config::{Config, ConfigError, LoadOptions, PresetOrigin, PresetStore};

fn load(directory: &std::path::Path, overrides: Option<&str>) -> Result<Config, ConfigError> {
    Config::load_strict(&LoadOptions {
        user_config_dir: Some(directory.to_path_buf()),
        read_env: false,
        cli_overrides: overrides.map(|text| toml::from_str(text).unwrap()),
        ..Default::default()
    })
}

#[test]
fn legacy_bindings_migrate_before_strict_validation_for_every_version() {
    for version in ["", "version = 1\n", "version = 2\n", "version = 3\n"] {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("config.toml"),
            format!(
                "{version}\
[agents.roles.oracle]
logical_model = 'consult'
preset = 'role-oracle'
[routing.routes]
consult = [{{ profile = 'p', reasoning_effort = 'high' }}]
"
            ),
        )
        .unwrap();
        let config = load(dir.path(), None).unwrap();
        let binding = config.agents.binding_for("advisor", None).unwrap();
        assert_eq!(binding, config.agents.binding_for("oracle", None).unwrap());
        assert_eq!(binding.models[0].route, "consult");
        assert_eq!(binding.models[0].reasoning_effort.as_deref(), Some("high"));
        assert_eq!(
            config.agents.roles.advisor.preset.as_deref(),
            Some("role-oracle")
        );
        let saved = dir.path().join("saved.toml");
        config::save_routing_and_agents(&saved, &config.routing, &config.agents).unwrap();
        let text = std::fs::read_to_string(saved).unwrap();
        assert!(text.contains("[agents.roles.advisor]"));
        assert!(!text.contains("[agents.roles.oracle]"));
    }
}

#[test]
fn alias_normalization_preserves_layer_precedence_and_profiles() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("config.toml"),
        "\
[agents.roles.advisor]
preset = 'lower'
[role_profiles.consult.agents.roles.oracle]
preset = 'role-oracle'
",
    )
    .unwrap();
    let config = load(
        dir.path(),
        Some("[agents.roles.oracle]\npreset = 'upper'\n"),
    )
    .unwrap();
    assert_eq!(config.agents.roles.advisor.preset.as_deref(), Some("upper"));
    assert_eq!(
        config.role_profiles["consult"]
            .agents
            .roles
            .advisor
            .preset
            .as_deref(),
        Some("role-oracle")
    );
    let config = load(
        dir.path(),
        Some("[agents.roles.oracle]\npreset = 'old'\n[agents.roles.advisor]\npreset = 'new'\n"),
    )
    .unwrap();
    assert_eq!(config.agents.roles.advisor.preset.as_deref(), Some("new"));
}

#[test]
fn implicit_legacy_route_keeps_its_identifier_and_chain_effort() {
    for effort in ["", ", reasoning_effort = 'high'"] {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("config.toml"),
            format!("[routing.routes]\noracle = [{{ profile = 'p'{effort} }}]\n"),
        )
        .unwrap();
        let config = load(dir.path(), None).unwrap();
        let binding = config.agents.binding_for("advisor", None).unwrap();
        assert_eq!(binding.models[0].route, "oracle");
        assert_eq!(
            binding.models[0].reasoning_effort.as_deref(),
            if effort.is_empty() {
                None
            } else {
                Some("high")
            }
        );
    }
}

#[test]
fn legacy_input_does_not_bypass_strict_unknown_field_or_category_checks() {
    let dir = tempfile::tempdir().unwrap();
    for field in ["typo", "categories"] {
        let error = load(
            dir.path(),
            Some(&format!("[agents.roles.oracle]\n{field} = {{}}\n")),
        )
        .unwrap_err();
        assert!(
            matches!(error, ConfigError::InvalidField { path, .. } if path == format!("agents.roles.advisor.{field}"))
        );
    }
}

#[test]
fn direct_serde_alias_writes_only_the_canonical_binding() {
    let config: Config = toml::from_str("[agents.roles.oracle]\npreset = 'role-oracle'\n").unwrap();
    let value = toml::Value::try_from(config).unwrap();
    assert!(value["agents"]["roles"].get("oracle").is_none());
    assert_eq!(
        value["agents"]["roles"]["advisor"]["preset"].as_str(),
        Some("role-oracle")
    );
}

#[test]
fn legacy_preset_fallback_uses_advisor_but_preserves_explicit_user_override() {
    let advisor = PresetStore::resolve("role-advisor", None).unwrap();
    assert_eq!(PresetStore::resolve("role-oracle", None).unwrap(), advisor);
    assert_eq!(
        PresetStore::origin("role-oracle", None),
        Some(PresetOrigin::Bundled)
    );
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("role-oracle.md");
    std::fs::write(&path, "custom consultation").unwrap();
    assert_eq!(
        PresetStore::resolve("role-oracle", Some(dir.path())).unwrap(),
        "custom consultation"
    );
    assert_eq!(
        PresetStore::origin("role-oracle", Some(dir.path())),
        Some(PresetOrigin::User(path))
    );
}

#[test]
fn dropins_and_environment_normalize_before_merging() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("config.toml"),
        "[agents.roles.oracle]\npreset = 'lower'\n",
    )
    .unwrap();
    std::fs::create_dir(dir.path().join("config.d")).unwrap();
    std::fs::write(
        dir.path().join("config.d/10-advisor.toml"),
        "[agents.roles.advisor]\npreset = 'upper'\n",
    )
    .unwrap();
    assert_eq!(
        load(dir.path(), None)
            .unwrap()
            .agents
            .roles
            .advisor
            .preset
            .as_deref(),
        Some("upper")
    );
    let config = Config::load_strict(&LoadOptions {
        user_config_dir: Some(dir.path().to_path_buf()),
        read_env: true,
        env: Some(std::collections::BTreeMap::from([(
            "EVORCH_AGENTS__ROLES__ORACLE__PRESET".into(),
            "environment".into(),
        )])),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(
        config.agents.roles.advisor.preset.as_deref(),
        Some("environment")
    );
}

#[test]
fn profile_legacy_chain_migrates_and_new_route_takes_precedence() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("config.toml"),
        "\
[role_profiles.consult.agents.roles.oracle]
logical_model = 'consult'
[role_profiles.consult.routing.routes]
consult = [{ profile = 'p', reasoning_effort = 'high' }]
[routing.routes]
oracle = [{ profile = 'p' }]
advisor = [{ profile = 'p' }]
",
    )
    .unwrap();
    let config = load(dir.path(), None).unwrap();
    let chain = &config.role_profiles["consult"].agents.roles.advisor.models;
    assert_eq!(chain[0].route, "consult");
    assert_eq!(chain[0].reasoning_effort.as_deref(), Some("high"));
    assert_eq!(
        config.agents.binding_for("advisor", None).unwrap().models[0].route,
        "advisor"
    );
}

#[test]
fn explicit_empty_legacy_chain_still_uses_legacy_route() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("config.toml"),
        "[agents.roles.oracle]\nmodels = []\n[routing.routes]\noracle = [{ profile = 'p' }]\n",
    )
    .unwrap();
    assert_eq!(
        load(dir.path(), None)
            .unwrap()
            .agents
            .binding_for("advisor", None)
            .unwrap()
            .models[0]
            .route,
        "oracle"
    );
}

#[test]
fn old_implicit_route_effort_does_not_override_new_implicit_route() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("config.toml"), "[routing.routes]\noracle = [{ profile = 'p', reasoning_effort = 'high' }]\nadvisor = [{ profile = 'p' }]\n").unwrap();
    let binding = load(dir.path(), None)
        .unwrap()
        .agents
        .binding_for("advisor", None)
        .unwrap();
    assert_eq!(binding.models[0].route, "advisor");
    assert_eq!(binding.models[0].reasoning_effort, None);
}

#[test]
fn route_only_layers_never_replace_explicit_advisor_chains() {
    for role in ["oracle", "advisor"] {
        for scope in ["", "role_profiles.consult."] {
            for route_on_top in [false, true] {
                for upper_layer in ["dropin", "env", "cli", "preview"] {
                    let dir = tempfile::tempdir().unwrap();
                    let binding = format!(
                        "[{scope}agents.roles.{role}]\nmodels = [{{route = 'custom', reasoning_effort = 'high'}}]\n[{scope}routing.routes]\ncustom = [{{profile = 'p'}}]\n"
                    );
                    let route = format!("[{scope}routing.routes]\noracle = [{{profile = 'p'}}]\n");
                    let (lower, upper) = if route_on_top {
                        (&binding, &route)
                    } else {
                        (&route, &binding)
                    };
                    std::fs::write(dir.path().join("config.toml"), lower).unwrap();
                    let mut opts = LoadOptions {
                        user_config_dir: Some(dir.path().to_path_buf()),
                        read_env: false,
                        ..Default::default()
                    };
                    match upper_layer {
                        "dropin" | "preview" => {
                            std::fs::create_dir(dir.path().join("config.d")).unwrap();
                            let path = dir.path().join("config.d/10-upper.toml");
                            std::fs::write(&path, upper).unwrap();
                            if upper_layer == "preview" {
                                opts.file_overrides
                                    .insert(path, toml::from_str(upper).unwrap());
                            }
                        }
                        "env" => {
                            opts.read_env = true;
                            let prefix =
                                format!("EVORCH_{}", scope.replace('.', "__").to_uppercase());
                            opts.env = Some(if route_on_top {
                                std::collections::BTreeMap::from([(
                                    format!("{prefix}ROUTING__ROUTES__ORACLE"),
                                    "[{profile = 'p'}]".into(),
                                )])
                            } else {
                                std::collections::BTreeMap::from([
                                    (
                                        format!(
                                            "{prefix}AGENTS__ROLES__{}__MODELS",
                                            role.to_uppercase()
                                        ),
                                        "[{route = 'custom', reasoning_effort = 'high'}]".into(),
                                    ),
                                    (
                                        format!("{prefix}ROUTING__ROUTES__CUSTOM"),
                                        "[{profile = 'p'}]".into(),
                                    ),
                                ])
                            });
                        }
                        "cli" => opts.cli_overrides = Some(toml::from_str(upper).unwrap()),
                        _ => unreachable!(),
                    }
                    let loaded = Config::load_strict(&opts).unwrap();
                    let agents = if scope.is_empty() {
                        &loaded.agents
                    } else {
                        &loaded.role_profiles["consult"].agents
                    };
                    let chain = agents.binding_for("advisor", None).unwrap().models;
                    assert_eq!(chain.len(), 1);
                    assert_eq!(
                        chain[0].route, "custom",
                        "{role}/{scope}/{route_on_top}/{upper_layer}"
                    );
                    assert_eq!(chain[0].reasoning_effort.as_deref(), Some("high"));
                }
            }
        }
    }
}

#[test]
fn empty_upper_chain_resets_to_implicit_legacy_route_after_merge() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("config.toml"), "[agents.roles.oracle]\nmodels = [{route = 'custom'}]\n[routing.routes]\ncustom = [{profile = 'p'}]\noracle = [{profile = 'p'}]\n").unwrap();
    let loaded = load(dir.path(), Some("[agents.roles.advisor]\nmodels = []\n")).unwrap();
    assert_eq!(
        loaded
            .agents
            .binding_for("advisor", None)
            .unwrap()
            .head_route(),
        "oracle"
    );
}
