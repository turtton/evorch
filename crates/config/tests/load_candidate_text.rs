//! The virtual candidate must have exactly the semantics of a real reload.

use std::path::Path;

use config::{Config, LoadOptions};

const PROVIDERS: &str = "[providers.codex]\ntype='openai-codex'\nmodels=['chat', {id='studio', enabled=true, kind='image_generation'}]\ndefault_model='chat'\n";

fn options(root: &Path) -> LoadOptions {
    LoadOptions {
        user_config_dir: Some(root.to_owned()),
        read_env: false,
        ..Default::default()
    }
}

fn diagnostics(config: &Config) -> Vec<String> {
    config
        .model_kind_problems()
        .iter()
        .map(ToString::to_string)
        .collect()
}

fn assert_reload_equivalent(
    main: &str,
    dropin: &str,
    target_dropin: bool,
    candidate: &str,
    overrides: bool,
) -> Config {
    let temp = tempfile::tempdir().unwrap();
    let main_path = temp.path().join("config.toml");
    let dropin_path = temp.path().join("config.d/50-edit.toml");
    std::fs::create_dir(temp.path().join("config.d")).unwrap();
    std::fs::write(&main_path, main).unwrap();
    std::fs::write(&dropin_path, dropin).unwrap();
    let path = if target_dropin {
        &dropin_path
    } else {
        &main_path
    };
    std::fs::write(path, "version=3\n[metrics]\nenabled=false\n").unwrap();
    let before = std::fs::read_to_string(path).unwrap();
    let mut opts = options(temp.path());
    if overrides {
        opts.read_env = true;
        opts.env = Some(
            [(
                "EVORCH_PROVIDERS__CODEX__MODELS".into(),
                "[{id='studio',enabled=true,kind='chat'},'chat']".into(),
            )]
            .into(),
        );
        opts.cli_overrides = Some(
            toml::from_str(
                "[providers.codex]\nmodels=['chat',{id='studio',enabled=true,kind='image_generation'}]",
            )
            .unwrap(),
        );
    }
    let virtual_config = Config::load_unresolved_with_file_text(&opts, path, candidate).unwrap();
    assert_eq!(std::fs::read_to_string(path).unwrap(), before);
    assert!(!path.with_extension("toml.tmp").exists());
    std::fs::write(path, candidate).unwrap();
    let disk_config = Config::load_unresolved(&opts).unwrap();
    assert_eq!(virtual_config, disk_config, "candidate:\n{candidate}");
    assert_eq!(diagnostics(&virtual_config), diagnostics(&disk_config));
    virtual_config
}

#[test]
fn candidate_text_matches_reload_across_layers_and_legacy_chain_migrations() {
    for version in [2, 3] {
        for target_dropin in [false, true] {
            for registration_in_dropin in [false, true] {
                for legacy in [false, true] {
                    let binding = if legacy {
                        "logical_model='pool'"
                    } else {
                        "models=[{route='pool'}]"
                    };
                    let chain = format!(
                        "version={version}\n[routing.routes]\npool=[{{profile='codex',model='studio'}}]\n[agents.worker]\n{binding}\n"
                    );
                    let providers = format!("version={version}\n{PROVIDERS}");
                    let main = if registration_in_dropin {
                        &chain
                    } else {
                        &providers
                    };
                    let dropin = if registration_in_dropin {
                        &providers
                    } else {
                        &chain
                    };
                    let candidate = if target_dropin { dropin } else { main };
                    let config =
                        assert_reload_equivalent(main, dropin, target_dropin, candidate, false);
                    assert_eq!(config.agents.worker.models[0].route, "pool");
                    assert!(
                        diagnostics(&config)
                            .iter()
                            .any(|error| error.contains("agents.worker.models"))
                    );
                }
            }
        }
    }
}

#[test]
fn candidate_text_matches_reload_for_advisor_oracle_provenance_and_upper_overrides() {
    for role in ["oracle", "advisor"] {
        for target_dropin in [false, true] {
            for explicit_upper_chain in [false, true] {
                let lower = format!(
                    "version=2\n{PROVIDERS}\n[routing.routes]\n{role}=[{{profile='codex',model='studio',reasoning_effort='high'}}]\nchat=[{{profile='codex',model='chat'}}]\n"
                );
                let upper = if explicit_upper_chain {
                    "version=3\n[agents.roles.advisor]\nmodels=[{route='chat'}]\n"
                } else {
                    "version=3\n"
                };
                let config = if target_dropin {
                    assert_reload_equivalent(&lower, "", true, upper, false)
                } else {
                    assert_reload_equivalent("", upper, false, &lower, false)
                };
                if explicit_upper_chain {
                    assert!(diagnostics(&config).is_empty());
                    assert_eq!(config.agents.roles.advisor.models[0].route, "chat");
                } else {
                    assert!(!diagnostics(&config).is_empty());
                }
            }
        }
    }
}

#[test]
fn candidate_text_matches_reload_for_all_kind_constraints_and_inactive_profiles() {
    for (name, scope, model, provider, models, extra_candidate, expected_problem) in [
        (
            "chat_to_image",
            "agents.worker",
            "studio",
            "openai-codex",
            "['chat',{id='studio',enabled=true,kind='image_generation'}]",
            "",
            true,
        ),
        (
            "image_to_chat",
            "agents.roles.image_generator",
            "chat",
            "openai-codex",
            "['chat']",
            "",
            true,
        ),
        (
            "explicit_overrides_inference",
            "agents.worker",
            "gpt-image-custom",
            "openai-codex",
            "['chat',{id='gpt-image-custom',enabled=true,kind='chat'}]",
            "",
            false,
        ),
        (
            "omitted_model",
            "agents.roles.image_generator",
            "",
            "openai-codex",
            "['chat']",
            "",
            true,
        ),
        (
            "mixed_route",
            "agents.worker",
            "chat",
            "openai-codex",
            "['chat',{id='studio',enabled=true,kind='image_generation'}]",
            ",{profile='codex',model='studio'}",
            true,
        ),
        (
            "unsupported_provider",
            "agents.roles.image_generator",
            "studio",
            "anthropic",
            "['chat',{id='studio',enabled=true,kind='image_generation'}]",
            "",
            true,
        ),
        (
            "after_rename_and_delete",
            "agents.roles.image_generator",
            "studio",
            "openai-codex",
            "['chat',{id='studio',enabled=true,kind='image_generation'}]",
            "",
            false,
        ),
    ] {
        for inactive in [false, true] {
            for overrides in [false, true] {
                let prefix = if inactive { "role_profiles.night." } else { "" };
                let model = if model.is_empty() {
                    String::new()
                } else {
                    format!(",model='{model}'")
                };
                let candidate = format!(
                    "version=3\n[{prefix}routing.routes]\nrenamed=[{{profile='codex'{model}}}{extra_candidate}]\n[{prefix}{scope}]\nmodels=[{{route='renamed'}}]\n"
                );
                let providers = format!(
                    "[providers.codex]\ntype='{provider}'\nmodels={models}\ndefault_model='chat'\n"
                );
                let config = assert_reload_equivalent("", &providers, false, &candidate, overrides);
                // Environment/CLI overrides intentionally replace the registration list.
                if !overrides {
                    assert_eq!(
                        !diagnostics(&config).is_empty(),
                        expected_problem,
                        "{name}, inactive={inactive}"
                    );
                }
            }
        }
    }
    for mode in ["env", "cli"] {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        std::fs::write(&path, "version=3").unwrap();
        let candidate = format!(
            "{PROVIDERS}\n[routing.routes]\npool=[{{profile='codex',model='studio'}}]\n[agents.worker]\nmodels=[{{route='pool'}}]\n"
        );
        let mut opts = options(temp.path());
        let models = "['chat',{id='studio',enabled=true,kind='chat'}]";
        if mode == "env" {
            opts.read_env = true;
            opts.env = Some([("EVORCH_PROVIDERS__CODEX__MODELS".into(), models.into())].into());
        } else {
            opts.cli_overrides =
                Some(toml::from_str(&format!("[providers.codex]\nmodels={models}")).unwrap());
        }
        let virtual_config =
            Config::load_unresolved_with_file_text(&opts, &path, &candidate).unwrap();
        std::fs::write(&path, &candidate).unwrap();
        let disk_config = Config::load_unresolved(&opts).unwrap();
        assert_eq!(virtual_config, disk_config);
        assert!(diagnostics(&virtual_config).is_empty(), "{mode}");
    }
}

#[test]
fn candidate_text_requires_a_discovered_target_but_supports_missing_main() {
    let temp = tempfile::tempdir().unwrap();
    let opts = options(temp.path());
    let main = temp.path().join("config.toml");
    let candidate = "version=2\n[agents.worker]\nlogical_model='pool'\n";
    let virtual_config = Config::load_unresolved_with_file_text(&opts, &main, candidate).unwrap();
    assert!(!main.exists());
    std::fs::write(&main, candidate).unwrap();
    assert_eq!(virtual_config, Config::load_unresolved(&opts).unwrap());
    for path in [
        temp.path().join("other.toml"),
        temp.path().join("config.d/new.toml"),
    ] {
        let error = Config::load_unresolved_with_file_text(&opts, &path, candidate).unwrap_err();
        assert!(error.to_string().contains("was not loaded"), "{error}");
        assert!(!path.exists());
    }
    let mut masked = opts;
    masked
        .file_overrides
        .insert(main.clone(), toml::from_str("version=3").unwrap());
    assert!(
        Config::load_unresolved_with_file_text(&masked, &main, candidate)
            .unwrap_err()
            .to_string()
            .contains("masked")
    );
}
