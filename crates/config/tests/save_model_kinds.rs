//! Save-boundary validation must use all layers, not infer kinds from one file.

use config::{Config, LoadOptions};

fn options(root: &std::path::Path) -> LoadOptions {
    LoadOptions {
        user_config_dir: Some(root.to_path_buf()),
        read_env: false,
        ..Default::default()
    }
}

const PROVIDERS: &str = r#"
[providers.codex]
type = "openai-codex"
models = ["chat", { id = "studio", enabled = true, kind = "image_generation" }]
default_model = "chat"
"#;

#[test]
fn saving_chains_accepts_model_registrations_in_dropin() {
    for profile in [None, Some("night")] {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        let prefix = profile.map_or_else(String::new, |name| format!("role_profiles.{name}."));
        let document = format!(
            "version = 3\n[{prefix}routing.routes]\nart = [{{ profile = 'codex', model = 'studio' }}]\n\
             [{prefix}agents.roles.image_generator]\nmodels = [{{ route = 'art' }}]\n"
        );
        std::fs::write(&path, document).unwrap();
        std::fs::create_dir(temp.path().join("config.d")).unwrap();
        let dropin = temp.path().join("config.d/providers.toml");
        std::fs::write(&dropin, PROVIDERS).unwrap();
        let options = options(temp.path());
        let config = Config::load_strict(&options).unwrap();
        let scope = config.role_profile_config(profile).unwrap();
        config::save_role_profile_bindings_with_options(
            &path,
            profile,
            Some(&scope.routing),
            Some(&scope.agents),
            &options,
        )
        .expect("explicit image registration from drop-in is used at save time");
        Config::load_strict(&options).unwrap();
        assert_eq!(std::fs::read_to_string(&dropin).unwrap(), PROVIDERS);

        // Saving an unrelated provider must not re-check the chain without drop-ins.
        config::save_codex_provider_edit_with_options(
            &path,
            &config::CodexProviderInput {
                name: "other".into(),
                account: "other".into(),
                base_url: "https://chatgpt.com/backend-api/codex".into(),
                models: vec![config::ModelEntryConfig::enabled("other-chat")],
                default_model: "other-chat".into(),
            },
            None,
            &options,
        )
        .expect("provider save uses the same layered boundary");
        Config::load_strict(&options).unwrap();
    }
}

#[test]
fn saving_chains_rejects_incompatible_dropin_kind_without_writing() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("config.toml");
    let document = "version = 3\n[routing.routes]\npool = [{ profile = 'codex', model = 'studio' }]\n\
                    [agents.worker]\nmodels = [{ route = 'pool' }]\n";
    std::fs::write(&path, document).unwrap();
    std::fs::create_dir(temp.path().join("config.d")).unwrap();
    std::fs::write(temp.path().join("config.d/providers.toml"), PROVIDERS).unwrap();
    let options = options(temp.path());
    let config = Config::load_unresolved(&options).unwrap();
    let error = config::save_role_profile_bindings_with_options(
        &path,
        None,
        Some(&config.routing),
        None,
        &options,
    )
    .unwrap_err();
    assert!(
        error.to_string().contains("agents.worker.models"),
        "{error}"
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), document);
    assert!(!path.with_extension("toml.tmp").exists());
}

#[test]
fn explicit_codex_models_require_enabled_chat_default_without_writing() {
    for layered in [false, true] {
        for case in ["missing", "image", "disabled"] {
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join("config.toml");
            std::fs::write(&path, PROVIDERS).unwrap();
            let mut chat = config::ModelEntryConfig::enabled("chat");
            chat.enabled = case != "disabled";
            let mut image = config::ModelEntryConfig::enabled("studio");
            image.kind = Some(config::ModelKind::ImageGeneration);
            let input = config::CodexProviderInput {
                name: "codex".into(),
                account: "codex".into(),
                base_url: "https://chatgpt.com/backend-api/codex".into(),
                models: vec![chat, image],
                default_model: match case {
                    "missing" => String::new(),
                    "image" => "studio".into(),
                    _ => "chat".into(),
                },
            };
            let result = if layered {
                config::save_codex_provider_edit_with_options(
                    &path,
                    &input,
                    None,
                    &options(temp.path()),
                )
            } else {
                config::save_codex_provider(&path, &input)
            };
            let error = result.unwrap_err();
            assert!(error.to_string().contains("default_model"), "{error}");
            assert_eq!(std::fs::read_to_string(&path).unwrap(), PROVIDERS);
            assert!(!path.with_extension("toml.tmp").exists());
            Config::load_strict(&options(temp.path())).unwrap();
        }
    }
}

#[test]
fn standalone_codex_api_can_still_use_builtin_models_and_default() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("config.toml");
    config::save_codex_provider(
        &path,
        &config::CodexProviderInput {
            name: "codex".into(),
            account: "codex".into(),
            base_url: "https://chatgpt.com/backend-api/codex".into(),
            models: Vec::new(),
            default_model: String::new(),
        },
    )
    .unwrap();
    let config = Config::load_strict(&options(temp.path())).unwrap();
    let profile = &config.providers["codex"];
    assert!(!profile.default_model.is_empty());
    assert!(
        profile
            .models
            .iter()
            .any(|model| model.id == profile.default_model)
    );
    assert_eq!(
        profile.model_kind(&profile.default_model),
        config::ModelKind::Chat
    );
}
