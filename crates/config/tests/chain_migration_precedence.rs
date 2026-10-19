use config::{Config, LoadOptions};

// The legacy per-file migration intentionally retains its existing cross-layer
// precedence. An unrelated route named `advisor` must not change that behavior.
#[test]
fn legacy_chain_migration_preserves_base_precedence() {
    for role in ["reviewer", "oracle"] {
        assert_explicit_chain_precedence(role);
    }
}

#[test]
fn canonical_advisor_chain_preserves_legacy_precedence() {
    assert_explicit_chain_precedence("advisor");
}

fn assert_explicit_chain_precedence(role: &str) {
    let legacy_role = if role == "advisor" { "oracle" } else { role };
    for route in [legacy_role, "advisor"] {
        for effort in [false, true] {
            for separate_layers in [false, true] {
                for scope in ["", "role_profiles.consult."] {
                    for legacy_binding in [false, true] {
                        let directory = tempfile::tempdir().unwrap();
                        let address = if matches!(role, "oracle" | "advisor") {
                            format!("roles.{role}")
                        } else {
                            role.to_owned()
                        };
                        let binding = if legacy_binding {
                            "logical_model = 'custom'"
                        } else {
                            "models = [{ route = 'custom', reasoning_effort = 'low' }]"
                        };
                        let main = format!(
                            "[{scope}agents.{address}]\n{binding}\n[{scope}routing.routes]\ncustom = [{{ profile = 'p' }}]\n"
                        );
                        let effort_field = if effort {
                            ", reasoning_effort = 'high'"
                        } else {
                            ""
                        };
                        let route_layer = format!(
                            "[{scope}routing.routes]\n{route} = [{{ profile = 'p'{effort_field} }}]\n"
                        );
                        if separate_layers {
                            std::fs::write(directory.path().join("config.toml"), main).unwrap();
                            std::fs::create_dir(directory.path().join("config.d")).unwrap();
                            std::fs::write(
                                directory.path().join("config.d/10-route.toml"),
                                route_layer,
                            )
                            .unwrap();
                        } else {
                            let route_entry =
                                route_layer.lines().skip(1).collect::<Vec<_>>().join("\n");
                            std::fs::write(
                                directory.path().join("config.toml"),
                                format!("{main}{route_entry}\n"),
                            )
                            .unwrap();
                        }
                        let loaded = Config::load_strict(&LoadOptions {
                            user_config_dir: Some(directory.path().to_path_buf()),
                            read_env: false,
                            ..Default::default()
                        })
                        .unwrap();
                        let agents = if scope.is_empty() {
                            &loaded.agents
                        } else {
                            &loaded.role_profiles["consult"].agents
                        };
                        let chain = agents.binding_for(role, None).unwrap().models;
                        let overwritten = separate_layers && effort && route == legacy_role;
                        let expected_route = if overwritten { legacy_role } else { "custom" };
                        let expected_effort = if overwritten {
                            Some("high")
                        } else if legacy_binding {
                            None
                        } else {
                            Some("low")
                        };
                        let case = format!(
                            "{role}/{route}/effort={effort}/separate={separate_layers}/{scope}/logical_model={legacy_binding}"
                        );
                        assert_eq!(chain.len(), 1, "{case}");
                        assert_eq!(chain[0].route, expected_route, "{case}");
                        assert_eq!(
                            chain[0].reasoning_effort.as_deref(),
                            expected_effort,
                            "{case}"
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn implicit_advisor_route_keeps_first_effort_and_cleans_every_candidate() {
    for role in [None, Some("oracle"), Some("advisor")] {
        for scope in ["", "role_profiles.consult."] {
            for effort in [false, true] {
                let directory = tempfile::tempdir().unwrap();
                let binding = role.map_or_else(String::new, |role| {
                    format!("[{scope}agents.roles.{role}]\nmodels = []\n")
                });
                let effort_field = if effort {
                    ", reasoning_effort = 'high'"
                } else {
                    ""
                };
                std::fs::write(directory.path().join("config.toml"), format!(
                    "{binding}[{scope}routing.routes]\nadvisor = [{{ profile = 'p'{effort_field} }}, {{ profile = 'q', reasoning_effort = 'low' }}]\n"
                )).unwrap();
                let loaded = Config::load_strict(&LoadOptions {
                    user_config_dir: Some(directory.path().to_path_buf()),
                    read_env: false,
                    ..Default::default()
                })
                .unwrap();
                let agents = if scope.is_empty() {
                    &loaded.agents
                } else {
                    &loaded.role_profiles["consult"].agents
                };
                let chain = agents.binding_for("advisor", None).unwrap().models;
                assert_eq!(chain.len(), 1);
                assert_eq!(chain[0].route, "advisor");
                assert_eq!(
                    chain[0].reasoning_effort.as_deref(),
                    effort.then_some("high")
                );
                let serialized = toml::Value::try_from(&loaded).unwrap();
                let serialized = if scope.is_empty() {
                    &serialized
                } else {
                    &serialized["role_profiles"]["consult"]
                };
                for candidate in serialized["routing"]["routes"]["advisor"]
                    .as_array()
                    .unwrap()
                {
                    assert!(candidate.get("reasoning_effort").is_none());
                }
            }
        }
    }
}

#[test]
fn deferred_advisor_effort_uses_the_winning_route_layer() {
    for upper_effort in [None, Some("low")] {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join("config.toml"),
            "[routing.routes]\nadvisor = [{ profile = 'p', reasoning_effort = 'high' }]\n",
        )
        .unwrap();
        std::fs::create_dir(directory.path().join("config.d")).unwrap();
        let effort = upper_effort.map_or_else(String::new, |effort| {
            format!(", reasoning_effort = '{effort}'")
        });
        std::fs::write(
            directory.path().join("config.d/10-route.toml"),
            format!("[routing.routes]\nadvisor = [{{ profile = 'q'{effort} }}]\n"),
        )
        .unwrap();
        let loaded = Config::load_strict(&LoadOptions {
            user_config_dir: Some(directory.path().to_path_buf()),
            read_env: false,
            ..Default::default()
        })
        .unwrap();
        let chain = loaded.agents.binding_for("advisor", None).unwrap().models;
        assert_eq!(chain[0].route, "advisor");
        assert_eq!(chain[0].reasoning_effort.as_deref(), upper_effort);
        assert_eq!(loaded.routing.routes["advisor"][0].profile, "q");
    }
}

#[test]
fn mixed_implicit_routes_follow_layer_precedence() {
    for (lower, upper) in [
        ("oracle", "advisor"),
        ("advisor", "oracle"),
        ("oracle", "oracle"),
        ("advisor", "advisor"),
    ] {
        for scope in ["", "role_profiles.consult."] {
            let directory = tempfile::tempdir().unwrap();
            std::fs::write(
                directory.path().join("config.toml"),
                format!(
                    "[{scope}routing.routes]\n{lower} = [{{ profile = 'p', reasoning_effort = 'low' }}]\n"
                ),
            )
            .unwrap();
            std::fs::create_dir(directory.path().join("config.d")).unwrap();
            std::fs::write(
                directory.path().join("config.d/10-upper.toml"),
                format!(
                    "[{scope}routing.routes]\n{upper} = [{{ profile = 'q', reasoning_effort = 'high' }}]\n"
                ),
            )
            .unwrap();
            let loaded = Config::load_strict(&LoadOptions {
                user_config_dir: Some(directory.path().to_path_buf()),
                read_env: false,
                ..Default::default()
            })
            .unwrap();
            let (agents, routing) = if scope.is_empty() {
                (&loaded.agents, &loaded.routing)
            } else {
                let profile = &loaded.role_profiles["consult"];
                (&profile.agents, &profile.routing)
            };
            let chain = agents.binding_for("advisor", None).unwrap().models;
            let case = format!("{lower} -> {upper} / {scope}");
            assert_eq!(chain.len(), 1, "{case}");
            assert_eq!(chain[0].route, upper, "{case}");
            assert_eq!(chain[0].reasoning_effort.as_deref(), Some("high"), "{case}");
            assert_eq!(routing.routes[upper][0].profile, "q", "{case}");
            if lower != upper {
                assert_eq!(routing.routes[lower][0].profile, "p", "{case}");
            }
        }
    }
}

#[test]
fn legacy_route_without_effort_does_not_generate_an_upper_chain() {
    for lower in ["oracle", "advisor"] {
        for scope in ["", "role_profiles.consult."] {
            let directory = tempfile::tempdir().unwrap();
            std::fs::write(
                directory.path().join("config.toml"),
                format!(
                    "[{scope}routing.routes]\n{lower} = [{{ profile = 'p', reasoning_effort = 'low' }}]\n"
                ),
            )
            .unwrap();
            std::fs::create_dir(directory.path().join("config.d")).unwrap();
            std::fs::write(
                directory.path().join("config.d/10-oracle.toml"),
                format!("[{scope}routing.routes]\noracle = [{{ profile = 'q' }}]\n"),
            )
            .unwrap();
            let loaded = Config::load_strict(&LoadOptions {
                user_config_dir: Some(directory.path().to_path_buf()),
                read_env: false,
                ..Default::default()
            })
            .unwrap();
            let (agents, routing) = if scope.is_empty() {
                (&loaded.agents, &loaded.routing)
            } else {
                let profile = &loaded.role_profiles["consult"];
                (&profile.agents, &profile.routing)
            };
            let chain = agents.binding_for("advisor", None).unwrap().models;
            let case = format!("{lower} -> oracle / {scope}");
            assert_eq!(chain.len(), 1, "{case}");
            assert_eq!(chain[0].route, lower, "{case}");
            assert_eq!(chain[0].reasoning_effort.as_deref(), Some("low"), "{case}");
            assert_eq!(routing.routes["oracle"][0].profile, "q", "{case}");
        }
    }
}

#[test]
fn new_implicit_route_without_effort_replaces_legacy_implicit_chain() {
    for scope in ["", "role_profiles.consult."] {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join("config.toml"),
            format!(
                "[{scope}routing.routes]\noracle = [{{ profile = 'p', reasoning_effort = 'low' }}]\n"
            ),
        )
        .unwrap();
        std::fs::create_dir(directory.path().join("config.d")).unwrap();
        std::fs::write(
            directory.path().join("config.d/10-advisor.toml"),
            format!("[{scope}routing.routes]\nadvisor = [{{ profile = 'q' }}]\n"),
        )
        .unwrap();
        let loaded = Config::load_strict(&LoadOptions {
            user_config_dir: Some(directory.path().to_path_buf()),
            read_env: false,
            ..Default::default()
        })
        .unwrap();
        let agents = if scope.is_empty() {
            &loaded.agents
        } else {
            &loaded.role_profiles["consult"].agents
        };
        let chain = agents.binding_for("advisor", None).unwrap().models;
        assert_eq!(chain.len(), 1, "{scope}");
        assert_eq!(chain[0].route, "advisor", "{scope}");
        assert_eq!(chain[0].reasoning_effort, None, "{scope}");
    }
}

#[test]
fn explicit_chain_replacing_implicit_chain_does_not_inherit_its_provenance() {
    for role in ["oracle", "advisor"] {
        for route in ["oracle", "custom"] {
            for scope in ["", "role_profiles.consult."] {
                let directory = tempfile::tempdir().unwrap();
                std::fs::write(
                    directory.path().join("config.toml"),
                    format!(
                        "[{scope}routing.routes]\noracle = [{{ profile = 'p', reasoning_effort = 'low' }}]\ncustom = [{{ profile = 'p' }}]\n"
                    ),
                )
                .unwrap();
                std::fs::create_dir(directory.path().join("config.d")).unwrap();
                // For route=oracle this is identical to the generated chain,
                // but its explicit origin must still protect it from advisor.
                std::fs::write(
                    directory.path().join("config.d/10-explicit.toml"),
                    format!(
                        "[{scope}agents.roles.{role}]\nmodels = [{{ route = '{route}', reasoning_effort = 'low' }}]\n"
                    ),
                )
                .unwrap();
                std::fs::write(
                    directory.path().join("config.d/20-advisor.toml"),
                    format!(
                        "[{scope}routing.routes]\nadvisor = [{{ profile = 'q', reasoning_effort = 'high' }}]\n"
                    ),
                )
                .unwrap();
                let loaded = Config::load_strict(&LoadOptions {
                    user_config_dir: Some(directory.path().to_path_buf()),
                    read_env: false,
                    ..Default::default()
                })
                .unwrap();
                let agents = if scope.is_empty() {
                    &loaded.agents
                } else {
                    &loaded.role_profiles["consult"].agents
                };
                let chain = agents.binding_for("advisor", None).unwrap().models;
                let case = format!("{role}/{route}/{scope}");
                assert_eq!(chain.len(), 1, "{case}");
                assert_eq!(chain[0].route, route, "{case}");
                assert_eq!(chain[0].reasoning_effort.as_deref(), Some("low"), "{case}");
            }
        }
    }
}

#[test]
fn standalone_save_materializes_legacy_implicit_chain() {
    for scope in ["", "role_profiles.consult."] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        std::fs::write(
            &path,
            format!(
                "version = 2\n[{scope}routing.routes]\noracle = [{{ profile = 'p', reasoning_effort = 'low' }}]\n"
            ),
        )
        .unwrap();
        config::save_sandbox(&path, config::SandboxConfig::default()).unwrap();
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(!saved.contains("__evorch_"), "{scope}: {saved}");
        // Once saved as an explicit chain, a later advisor route must not replace it.
        std::fs::create_dir(directory.path().join("config.d")).unwrap();
        std::fs::write(
            directory.path().join("config.d/10-advisor.toml"),
            format!(
                "[{scope}routing.routes]\nadvisor = [{{ profile = 'q', reasoning_effort = 'high' }}]\n"
            ),
        )
        .unwrap();
        let loaded = Config::load_strict(&LoadOptions {
            user_config_dir: Some(directory.path().to_path_buf()),
            read_env: false,
            ..Default::default()
        })
        .unwrap();
        let agents = if scope.is_empty() {
            &loaded.agents
        } else {
            &loaded.role_profiles["consult"].agents
        };
        let chain = agents.binding_for("advisor", None).unwrap().models;
        assert_eq!(chain.len(), 1, "{scope}");
        assert_eq!(chain[0].route, "oracle", "{scope}");
        assert_eq!(chain[0].reasoning_effort.as_deref(), Some("low"), "{scope}");
    }
}

#[test]
fn standalone_v2_save_retains_deferred_implicit_effort() {
    for scope in ["", "role_profiles.consult."] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        std::fs::write(&path, format!("version = 2\n[{scope}routing.routes]\nadvisor = [{{ profile = 'p', reasoning_effort = 'high' }}]\n")).unwrap();
        // Saving an unrelated section also upgrades the original v2 document.
        config::save_sandbox(&path, config::SandboxConfig::default()).unwrap();
        let loaded = Config::load_strict(&LoadOptions {
            user_config_dir: Some(directory.path().to_path_buf()),
            read_env: false,
            ..Default::default()
        })
        .unwrap();
        let agents = if scope.is_empty() {
            &loaded.agents
        } else {
            &loaded.role_profiles["consult"].agents
        };
        let chain = agents.binding_for("advisor", None).unwrap().models;
        assert_eq!(chain[0].route, "advisor");
        assert_eq!(chain[0].reasoning_effort.as_deref(), Some("high"));
        let saved: toml::Value = toml::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let saved = if scope.is_empty() {
            &saved
        } else {
            &saved["role_profiles"]["consult"]
        };
        assert!(
            saved["routing"]["routes"]["advisor"][0]
                .get("reasoning_effort")
                .is_none()
        );
    }
}
