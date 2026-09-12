use config::{
    AgentsConfig, CategoryBindingConfig, Config, GenerationOverridesConfig, LoadOptions,
    RoleBindingConfig, save_agent_bindings,
};

fn load(directory: &std::path::Path) -> Config {
    Config::load(&LoadOptions {
        project_dir: Some(directory.to_path_buf()),
        user_config_dir: Some(directory.join("user-empty")),
        read_env: false,
        ..LoadOptions::default()
    })
    .expect("保存結果を読み込める")
}

#[test]
fn save_role_binding_roundtrips_logical_model_and_category_overrides() {
    // Given: worker role と quick category に異なる binding を設定する
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("evorch.toml");
    let agents = AgentsConfig {
        worker: RoleBindingConfig {
            logical_model: Some("worker-model".into()),
            preset: Some("worker-preset".into()),
            generation: GenerationOverridesConfig {
                temperature: Some(0.2),
                ..GenerationOverridesConfig::default()
            },
            categories: [(
                "quick".into(),
                CategoryBindingConfig {
                    logical_model: Some("quick-model".into()),
                    preset: Some("quick-preset".into()),
                    generation: GenerationOverridesConfig {
                        max_tokens: Some(4096),
                        ..GenerationOverridesConfig::default()
                    },
                },
            )]
            .into_iter()
            .collect(),
        },
        ..AgentsConfig::default()
    };

    // When: agents section を保存してロードする
    save_agent_bindings(&path, &agents).expect("agent bindings save succeeds");
    let loaded = load(directory.path());

    // Then: role と category の各 override が binding_for に反映される
    let role = loaded
        .agents
        .binding_for("worker", None)
        .expect("worker binding");
    assert_eq!(role.logical_model, "worker-model");
    assert_eq!(role.preset.as_deref(), Some("worker-preset"));
    assert_eq!(role.generation.temperature, Some(0.2));

    let quick = loaded
        .agents
        .binding_for("worker", Some("quick"))
        .expect("worker quick binding");
    assert_eq!(quick.logical_model, "quick-model");
    assert_eq!(quick.preset.as_deref(), Some("quick-preset"));
    assert_eq!(quick.generation.temperature, Some(0.2));
    assert_eq!(quick.generation.max_tokens, Some(4096));
}

#[test]
fn save_agent_bindings_preserves_existing_unrelated_config_sections() {
    // Given: providers を含む既存設定
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("evorch.toml");
    std::fs::write(
        &path,
        "version = 2\n\n[providers.other]\ntype = 'openai-compatible'\nbase_url = 'https://example.com/v1'\napi_key_env = 'OTHER_KEY'\nmodels = ['model-a']\ndefault_model = 'model-a'\n",
    )
    .expect("write initial config");
    let agents = AgentsConfig {
        worker: RoleBindingConfig {
            logical_model: Some("worker-model".into()),
            ..RoleBindingConfig::default()
        },
        ..AgentsConfig::default()
    };

    // When: agents section だけを保存する
    save_agent_bindings(&path, &agents).expect("agent bindings save succeeds");
    let loaded = load(directory.path());

    // Then: provider は変更されず agents だけ更新される
    assert_eq!(loaded.providers["other"].default_model, "model-a");
    assert_eq!(
        loaded
            .agents
            .binding_for("worker", None)
            .expect("worker")
            .logical_model,
        "worker-model"
    );
}
