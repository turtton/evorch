//! All public context save operations share the same final-state contract.

use std::path::Path;

use config::{Config, LayeredSaveContext, LoadOptions, RoleProfileBindings};

const PROVIDER: &str = "[providers.codex]\ntype='openai-codex'\nmodels=['chat',{id='studio',enabled=true,kind='image_generation'}]\ndefault_model='chat'\n";
const CHAIN: &str = "version=3\n[routing.routes]\nart=[{profile='codex',model='studio'}]\n[agents.roles.image_generator]\nmodels=[{route='art'}]\n[role_profiles.spare]\n";
const INVALID: &str = "[role_profiles.night.routing.routes]\npool=[{profile='codex',model='studio'}]\n[role_profiles.night.agents.worker]\nmodels=[{route='pool'}]\n";

fn options(root: &Path) -> LoadOptions {
    LoadOptions {
        user_config_dir: Some(root.to_owned()),
        read_env: false,
        ..Default::default()
    }
}

#[test]
fn every_layered_save_and_preview_accepts_split_config_and_rejects_inactive_kind_errors_atomically()
{
    for operation in [
        "provider",
        "provider_rename",
        "codex",
        "subscription",
        "delete_provider",
        "bindings",
        "multi_bindings",
        "preview",
        "prepare",
        "profile",
        "delete_profile",
        "routing",
        "agents",
        "routing_agents",
        "sandbox",
        "agent_features",
        "self_improvement",
        "project",
    ] {
        for invalid in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join("config.toml");
            let document = format!(
                "{CHAIN}{}\n[providers.unused]\ntype='openai-codex'\nmodels=['chat']\ndefault_model='chat'\n",
                if invalid { INVALID } else { "" }
            );
            std::fs::write(&path, &document).unwrap();
            std::fs::create_dir(temp.path().join("config.d")).unwrap();
            let dropin = temp.path().join("config.d/providers.toml");
            std::fs::write(&dropin, PROVIDER).unwrap();
            let mut options = options(temp.path());
            if operation == "project" {
                options.project_dir = Some(temp.path().to_owned());
            }
            let config = Config::load_unresolved(&options).unwrap();
            let context = LayeredSaveContext::new(&options);
            let changes = [RoleProfileBindings {
                profile: None,
                routing: Some(&config.routing),
                agents: Some(&config.agents),
            }];
            let input = config::OpenAiCompatibleProviderInput {
                name: "other".into(),
                provider_type: config::ProviderTypeConfig::OpenAiCompatible,
                base_url: "https://example.invalid/v1".into(),
                credential: config::ProviderCredentialInput::Env {
                    var: "TEST_KEY".into(),
                },
                models: vec![config::ModelEntryConfig::enabled("chat")],
                excluded_models: vec![],
                default_model: "chat".into(),
            };
            let preview = context.preview_role_profiles_bindings(&path, &changes);
            let result = match operation {
                "provider" | "provider_rename" => context.save_openai_compatible_provider_edit(
                    &path,
                    &input,
                    (operation == "provider_rename").then_some("unused"),
                ),
                "codex" => context.save_codex_provider_edit(
                    &path,
                    &config::CodexProviderInput {
                        name: "other".into(),
                        account: "other".into(),
                        base_url: "https://chatgpt.com/backend-api/codex".into(),
                        models: input.models.clone(),
                        default_model: "chat".into(),
                    },
                    None,
                    config::CodexModelIntent::ExplicitModels,
                ),
                "subscription" => context.save_subscription_provider_edit(
                    &path,
                    &config::SubscriptionProviderInput {
                        name: "other".into(),
                        provider_type: config::ProviderTypeConfig::Cursor,
                        account: "other".into(),
                        base_url: input.base_url.clone(),
                        models: input.models.clone(),
                        excluded_models: vec![],
                        default_model: "chat".into(),
                    },
                    None,
                ),
                "delete_provider" => context.delete_provider(&path, "unused"),
                "bindings" => context.save_role_profile_bindings(
                    &path,
                    None,
                    Some(&config.routing),
                    Some(&config.agents),
                ),
                "multi_bindings" => context.save_role_profiles_bindings(&path, &changes),
                "preview" => preview
                    .as_ref()
                    .map(|text| {
                        assert!(text.contains("art"));
                    })
                    .map_err(|error| config::ConfigError::Migration(error.to_string())),
                "prepare" => context
                    .prepare_role_profiles_bindings(&path, &changes)
                    .and_then(|prepared| {
                        assert_eq!(prepared.config(), &config);
                        prepared.write()
                    }),
                "profile" => context.save_role_profile(
                    &path,
                    "copy",
                    &config.role_profile_config(None).unwrap(),
                ),
                "delete_profile" => context.delete_role_profile(&path, "spare"),
                "routing" => context.save_routing(&path, &config.routing),
                "agents" => context.save_agent_bindings(&path, &config.agents),
                "routing_agents" => {
                    context.save_routing_and_agents(&path, &config.routing, Some(&config.agents))
                }
                "sandbox" => context.save_sandbox(&path, config.sandbox),
                "agent_features" => {
                    context.save_agent_features(&path, config::AgentFeatures::from_config(&config))
                }
                "self_improvement" => {
                    context.save_self_improvement(&path, &config.self_improvement)
                }
                "project" => context.save_project_role_profile(temp.path(), Some("spare")),
                _ => unreachable!(),
            };
            if invalid {
                let error = result
                    .err()
                    .unwrap_or_else(|| panic!("{operation} accepted invalid inactive profile"));
                assert!(
                    error
                        .to_string()
                        .contains("role_profiles.night.agents.worker.models"),
                    "{operation}: {error}"
                );
                assert_eq!(
                    std::fs::read_to_string(&path).unwrap(),
                    document,
                    "{operation}"
                );
                assert!(!config::project_main_config_path(temp.path()).exists());
            } else {
                result.unwrap_or_else(|error| panic!("{operation}: {error}"));
                Config::load_strict(&options).unwrap();
                if matches!(operation, "provider_rename" | "delete_provider") {
                    assert!(
                        !Config::load_unresolved(&options)
                            .unwrap()
                            .providers
                            .contains_key("unused")
                    );
                }
                if matches!(operation, "bindings" | "multi_bindings" | "prepare") {
                    assert_eq!(std::fs::read_to_string(&path).unwrap(), preview.unwrap());
                }
            }
            assert!(!path.with_extension("toml.tmp").exists(), "{operation}");
            assert!(
                !config::project_main_config_path(temp.path())
                    .with_extension("toml.tmp")
                    .exists()
            );
            assert_eq!(std::fs::read_to_string(&dropin).unwrap(), PROVIDER);
        }
    }
}

#[test]
fn legacy_v3_logical_model_kind_change_is_rejected_without_touching_original() {
    for version in [2, 3] {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        let document = format!(
            "version={version}\n[providers.codex]\ntype='openai-codex'\nmodels=['chat','studio']\ndefault_model='chat'\n[routing.routes]\npool=[{{profile='codex',model='studio'}}]\n[agents.worker]\nlogical_model='pool'\n"
        );
        std::fs::write(&path, &document).unwrap();
        let options = options(temp.path());
        let mut image = config::ModelEntryConfig::enabled("studio");
        image.kind = Some(config::ModelKind::ImageGeneration);
        let error = LayeredSaveContext::new(&options)
            .save_codex_provider_edit(
                &path,
                &config::CodexProviderInput {
                    name: "codex".into(),
                    account: "codex".into(),
                    base_url: "https://chatgpt.com/backend-api/codex".into(),
                    models: vec![config::ModelEntryConfig::enabled("chat"), image],
                    default_model: "chat".into(),
                },
                None,
                config::CodexModelIntent::ExplicitModels,
            )
            .unwrap_err();
        assert!(
            error.to_string().contains("agents.worker.models"),
            "{error}"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), document);
        assert!(!path.with_extension("toml.tmp").exists());
    }
}

#[test]
fn standalone_contract_does_not_discover_adjacent_layers_and_codex_intent_is_independent_of_scope()
{
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("config.toml");
    std::fs::write(&path, "version=3\n").unwrap();
    std::fs::create_dir(temp.path().join("config.d")).unwrap();
    std::fs::write(temp.path().join("config.d/broken.toml"), "not valid TOML").unwrap();
    config::StandaloneSaveContext
        .save_sandbox(&path, config::SandboxConfig::default())
        .unwrap();
    std::fs::remove_file(temp.path().join("config.d/broken.toml")).unwrap();
    let options = options(temp.path());
    let input = config::CodexProviderInput {
        name: "other".into(),
        account: "other".into(),
        base_url: "https://chatgpt.com/backend-api/codex".into(),
        models: vec![],
        default_model: String::new(),
    };
    for layered in [false, true] {
        let before = std::fs::read_to_string(&path).unwrap();
        let explicit = if layered {
            LayeredSaveContext::new(&options).save_codex_provider_edit(
                &path,
                &input,
                None,
                config::CodexModelIntent::ExplicitModels,
            )
        } else {
            config::StandaloneSaveContext.save_codex_provider_edit(
                &path,
                &input,
                None,
                config::CodexModelIntent::ExplicitModels,
            )
        };
        assert!(
            explicit
                .unwrap_err()
                .to_string()
                .contains("chat default_model")
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        if layered {
            LayeredSaveContext::new(&options).save_codex_provider_edit(
                &path,
                &input,
                None,
                config::CodexModelIntent::BuiltinDefaults,
            )
        } else {
            config::StandaloneSaveContext.save_codex_provider_edit(
                &path,
                &input,
                None,
                config::CodexModelIntent::BuiltinDefaults,
            )
        }
        .unwrap();
        Config::load_strict(&options).unwrap();
    }
}

#[test]
fn save_boundary_keeps_undefined_route_policy_and_rejects_unread_targets() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("config.toml");
    let options = options(temp.path());
    let context = LayeredSaveContext::new(&options);
    let mut agents = config::AgentsConfig::default();
    agents.worker.base.models = vec![config::ChainEntryConfig::route("undefined")];
    context
        .save_agent_bindings(&path, &agents)
        .expect("undefined routes remain warnings, not model-kind errors");
    assert!(Config::load_strict(&options).is_err());
    let config = Config::load_unresolved(&options).unwrap();
    assert!(config.model_kind_problems().is_empty());
    for target in [
        temp.path().join("unread.toml"),
        temp.path().join("config.d/new.toml"),
    ] {
        let error = context
            .save_sandbox(&target, config::SandboxConfig::default())
            .unwrap_err();
        assert!(error.to_string().contains("was not loaded"), "{error}");
        assert!(!target.exists());
        assert!(!target.with_extension("toml.tmp").exists());
    }
    let before = std::fs::read_to_string(&path).unwrap();
    let blocked = path.with_extension("toml.tmp");
    std::fs::write(&blocked, "do not overwrite").unwrap();
    assert!(
        context
            .save_sandbox(&path, config::SandboxConfig::default())
            .is_err()
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    assert_eq!(
        std::fs::read_to_string(&blocked).unwrap(),
        "do not overwrite"
    );
}
