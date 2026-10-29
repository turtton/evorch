//! Regressions for validating the final layered settings before touching the file.

use std::path::Path;
use std::time::{Duration, Instant};

use gui::{app::WorkbenchState, fixture::DemoSource};

fn options(root: &Path) -> config::LoadOptions {
    config::LoadOptions {
        user_config_dir: Some(root.to_path_buf()),
        read_env: false,
        ..Default::default()
    }
}

fn state(root: &Path) -> WorkbenchState<DemoSource> {
    WorkbenchState::new(DemoSource(Vec::new()), &workspace_ui::UiSettings::default())
        .unwrap()
        .with_settings_load_options(options(root))
        .with_provider_settings_path(root.join("config.toml"))
        .with_provider_settings(
            gui::model::provider_settings::ProviderSettingsModel::seed_from_config(
                &config::Config::load_unresolved(&options(root)).unwrap(),
            ),
        )
}

fn finish_routing(state: &mut WorkbenchState<DemoSource>) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while state.routing_settings().is_saving() {
        assert!(Instant::now() < deadline, "routing save timed out");
        state.poll_routing_save();
        std::thread::yield_now();
    }
}

const PROVIDER: &str = r#"
[providers.codex]
type = "openai-codex"
models = ["chat", { id = "studio", enabled = true, kind = "image_generation" }]
default_model = "chat"
"#;

#[test]
fn gui_saves_image_chain_with_model_registration_in_dropin() {
    for role_save in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        std::fs::write(
            &path,
            "version = 3\n[routing.routes]\nart = [{ profile = 'codex', model = 'studio' }]\n\
             [agents.roles.image_generator]\nmodels = [{ route = 'art' }]\n",
        )
        .unwrap();
        std::fs::create_dir(temp.path().join("config.d")).unwrap();
        std::fs::write(temp.path().join("config.d/providers.toml"), PROVIDER).unwrap();
        config::Config::load_strict(&options(temp.path())).unwrap();
        let mut state = state(temp.path());
        if role_save {
            state.open_role_settings();
            state.submit_role_settings();
            let deadline = Instant::now() + Duration::from_secs(10);
            while state.role_settings().is_saving() {
                assert!(Instant::now() < deadline, "role save timed out");
                state.poll_role_save();
                std::thread::yield_now();
            }
            assert_eq!(state.role_settings().error, None);
        } else {
            state.open_routing_settings();
            state.submit_routing_settings();
            finish_routing(&mut state);
            assert_eq!(state.routing_settings().validation_error, None);
        }
        config::Config::load_strict(&options(temp.path())).unwrap();
    }
}

fn reject_last_codex_chat_edit(remove: bool) {
    // Include the all-models-deleted case: the GUI must not silently opt back into
    // built-in defaults even though the standalone API permits that explicit use.
    for image_remains in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        let document = if image_remains {
            PROVIDER.to_owned()
        } else {
            PROVIDER.replace(
                "[\"chat\", { id = \"studio\", enabled = true, kind = \"image_generation\" }]",
                "[\"chat\"]",
            )
        };
        std::fs::write(&path, &document).unwrap();
        config::Config::load_strict(&options(temp.path())).unwrap();
        let mut state = state(temp.path());
        state.open_provider_settings();
        state.provider_settings_mut().edit("codex");
        let editor = state.provider_settings_mut().codex_mut().unwrap();
        if remove {
            editor.remove_model(0);
        } else {
            editor.set_model_kind("chat", config::ModelKind::ImageGeneration);
        }
        assert!(editor.default_model.is_empty());
        state.submit_provider_settings();
        let deadline = Instant::now() + Duration::from_secs(10);
        while state.provider_settings().error.is_none() {
            assert!(
                Instant::now() < deadline,
                "provider save did not reject edit"
            );
            state.poll_provider_save();
            std::thread::yield_now();
        }
        let error = state.provider_settings().error.as_deref().unwrap();
        assert!(error.contains("chat default_model"), "{error}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), document);
        assert!(!path.with_extension("toml.tmp").exists());
        config::Config::load_strict(&options(temp.path())).unwrap();
    }
}

#[test]
fn codex_last_chat_kind_change_keeps_original_file() {
    reject_last_codex_chat_edit(false);
}

#[test]
fn codex_last_chat_removal_keeps_original_file() {
    reject_last_codex_chat_edit(true);
}

fn reject_route_replacement(image_generator: bool, unsupported_provider: bool) {
    for profile in ["default", "night"] {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        let prefix = if profile == "default" {
            String::new()
        } else {
            format!("role_profiles.{profile}.")
        };
        let (old_model, new_model) = if image_generator {
            (
                "studio",
                if unsupported_provider {
                    "studio"
                } else {
                    "chat"
                },
            )
        } else {
            ("chat", "studio")
        };
        let replacement_profile = if unsupported_provider {
            "local"
        } else {
            "codex"
        };
        let document = format!(
            "version = 3\n{PROVIDER}\n\
             [providers.local]\ntype = 'openai-compatible'\nbase_url = 'https://example.invalid/v1'\n\
             api_key_env = 'TEST_KEY'\nmodels = ['chat', {{ id = 'studio', enabled = true, kind = 'image_generation' }}]\n\
             default_model = 'chat'\n\
             [{prefix}routing.routes]\n\
             pool = [{{ profile = 'codex', model = '{old_model}' }}]\n\
             replacement = [{{ profile = '{replacement_profile}', model = '{new_model}' }}]\n"
        );
        std::fs::write(&path, &document).unwrap();
        let binding = if image_generator {
            "roles.image_generator"
        } else {
            "worker"
        };
        let dropin_document =
            format!("[{prefix}agents.{binding}]\nmodels = [{{ route = 'pool' }}]\n");
        std::fs::create_dir(temp.path().join("config.d")).unwrap();
        let dropin = temp.path().join("config.d/chains.toml");
        std::fs::write(&dropin, &dropin_document).unwrap();
        config::Config::load_strict(&options(temp.path())).unwrap();
        let mut state = state(temp.path());
        state.open_routing_settings();
        if profile != "default" {
            state.apply_routing_profile_action(
                gui::model::role_profiles::RoleProfileAction::Select(profile.into()),
            );
        }
        let routing = state.routing_settings_mut();
        routing.routes.remove("pool");
        routing.rename_route("replacement", "pool").unwrap();
        // The old-name check alone accepts the draft; the final layered boundary
        // must notice that the drop-in chain now resolves to the replacement.
        routing.validated_routing().unwrap();
        state.submit_routing_settings();
        finish_routing(&mut state);
        let error = state
            .routing_settings()
            .validation_error
            .as_deref()
            .unwrap();
        assert!(
            error.contains(&format!("agents.{binding}.models")),
            "{error}"
        );
        if unsupported_provider {
            assert!(error.contains("cannot generate images"), "{error}");
        }
        assert_eq!(std::fs::read_to_string(&path).unwrap(), document);
        assert_eq!(std::fs::read_to_string(&dropin).unwrap(), dropin_document);
        assert!(!path.with_extension("toml.tmp").exists());
        config::Config::load_strict(&options(temp.path())).unwrap();
    }
}

#[test]
fn route_delete_then_rename_rejects_image_route_for_dropin_worker() {
    reject_route_replacement(false, false);
}

#[test]
fn route_delete_then_rename_rejects_chat_route_for_dropin_image_generator() {
    reject_route_replacement(true, false);
}

#[test]
fn route_delete_then_rename_rejects_unsupported_image_provider_for_dropin_chain() {
    reject_route_replacement(true, true);
}

fn finish_roles(state: &mut WorkbenchState<DemoSource>) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while state.role_settings().is_saving() {
        assert!(Instant::now() < deadline, "role save timed out");
        state.poll_role_save();
        std::thread::yield_now();
    }
}

fn split_image_fixture(root: &Path, invalid: bool) -> String {
    let document = format!(
        "version=3\n[routing.routes]\nart=[{{profile='codex',model='studio'}}]\n[agents.roles.image_generator]\nmodels=[{{route='art'}}]\n{}",
        if invalid {
            "[role_profiles.night.routing.routes]\npool=[{profile='codex',model='studio'}]\n[role_profiles.night.agents.worker]\nmodels=[{route='pool'}]\n"
        } else {
            ""
        }
    );
    std::fs::write(root.join("config.toml"), &document).unwrap();
    std::fs::create_dir(root.join("config.d")).unwrap();
    std::fs::write(root.join("config.d/providers.toml"), PROVIDER).unwrap();
    document
}

#[test]
fn gui_profile_create_delete_and_provider_delete_use_layered_context() {
    use gui::model::role_profiles::RoleProfileAction;
    for routing in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        split_image_fixture(temp.path(), false);
        let mut state = state(temp.path());
        for create in [true, false] {
            let action = if create {
                RoleProfileAction::Create("copy".into())
            } else {
                RoleProfileAction::Delete("copy".into())
            };
            if routing {
                state.open_routing_settings();
                state.apply_routing_profile_action(action);
                finish_routing(&mut state);
                assert_eq!(state.routing_settings().validation_error, None);
            } else {
                state.open_role_settings();
                state.apply_role_profile_action(action);
                finish_roles(&mut state);
                assert_eq!(state.role_settings().error, None);
            }
            let loaded = config::Config::load_strict(&options(temp.path())).unwrap();
            assert_eq!(loaded.role_profiles.contains_key("copy"), create);
        }
        // Remove a real but unused provider while the image registration is in a drop-in.
        config::LayeredSaveContext::new(&options(temp.path()))
            .save_codex_provider_edit(
                &temp.path().join("config.toml"),
                &config::CodexProviderInput {
                    name: "unused".into(),
                    account: "unused".into(),
                    base_url: "https://chatgpt.com/backend-api/codex".into(),
                    models: vec![],
                    default_model: String::new(),
                },
                None,
                config::CodexModelIntent::BuiltinDefaults,
            )
            .unwrap();
        let mut state = self::state(temp.path());
        state.delete_provider_settings("unused".into());
        let mut harness = gui::headless::HeadlessWorkbench::new(state, [1200.0, 900.0]);
        harness.wait_provider_save(Duration::from_secs(10));
        assert_eq!(harness.state().provider_settings().error, None);
        assert!(
            !config::Config::load_strict(&options(temp.path()))
                .unwrap()
                .providers
                .contains_key("unused")
        );
    }
}

#[test]
fn gui_image_route_rename_updates_references_with_registration_in_dropin() {
    let temp = tempfile::tempdir().unwrap();
    split_image_fixture(temp.path(), false);
    let mut state = state(temp.path());
    state.open_routing_settings();
    state
        .routing_settings_mut()
        .rename_route("art", "painting")
        .unwrap();
    state.submit_routing_settings();
    finish_routing(&mut state);
    assert_eq!(state.routing_settings().validation_error, None);
    let loaded = config::Config::load_strict(&options(temp.path())).unwrap();
    assert!(!loaded.routing.routes.contains_key("art"));
    assert!(loaded.routing.routes.contains_key("painting"));
    assert_eq!(
        loaded.agents.roles.image_generator.models[0].route,
        "painting"
    );
}

#[test]
fn gui_legacy_logical_model_kind_edit_is_rejected_before_write() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("config.toml");
    let document = "version=3\n[providers.codex]\ntype='openai-codex'\nmodels=['chat','studio']\ndefault_model='chat'\n[routing.routes]\npool=[{profile='codex',model='studio'}]\n[agents.worker]\nlogical_model='pool'\n";
    std::fs::write(&path, document).unwrap();
    let mut state = state(temp.path());
    state.open_provider_settings();
    state.provider_settings_mut().edit("codex");
    state
        .provider_settings_mut()
        .codex_mut()
        .unwrap()
        .set_model_kind("studio", config::ModelKind::ImageGeneration);
    state.submit_provider_settings();
    let mut harness = gui::headless::HeadlessWorkbench::new(state, [1200.0, 900.0]);
    harness.wait_provider_save(Duration::from_secs(10));
    let error = harness
        .state()
        .provider_settings()
        .error
        .as_deref()
        .unwrap();
    assert!(error.contains("agents.worker.models"), "{error}");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), document);
    assert!(!path.with_extension("toml.tmp").exists());
}

#[test]
fn gui_unrelated_settings_actions_validate_all_profiles_without_losing_layer_context() {
    for invalid in [false, true] {
        for sandbox in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let document = split_image_fixture(temp.path(), invalid);
            let path = temp.path().join("config.toml");
            let mut harness =
                gui::headless::HeadlessWorkbench::new(state(temp.path()), [1200.0, 900.0]);
            if sandbox {
                harness.state_mut().open_sandbox_settings();
                harness.run();
                harness.click_label("ask me");
                harness.click_label("Save sandbox");
                harness.run();
                assert_eq!(harness.has_label("Save sandbox"), invalid);
            } else {
                harness.state_mut().open_self_improvement_settings();
                harness.run();
                harness.click_label("Enable self-improvement drafts (default off)");
                harness.click_label("Save self-improvement");
                let expected_error = config::Config::load_unresolved(&options(temp.path()))
                    .unwrap()
                    .model_kind_problems()
                    .first()
                    .map(ToString::to_string);
                let deadline = Instant::now() + Duration::from_secs(10);
                loop {
                    harness.run();
                    let loaded = config::Config::load_unresolved(&options(temp.path())).unwrap();
                    if loaded.self_improvement.enabled
                        || expected_error
                            .as_ref()
                            .is_some_and(|error| harness.has_label(error))
                    {
                        break;
                    }
                    assert!(Instant::now() < deadline, "self-improvement save timed out");
                    std::thread::yield_now();
                }
            }
            if invalid {
                assert_eq!(std::fs::read_to_string(&path).unwrap(), document);
            } else {
                config::Config::load_strict(&options(temp.path())).unwrap();
                assert_ne!(std::fs::read_to_string(&path).unwrap(), document);
            }
            assert!(!path.with_extension("toml.tmp").exists());
        }
    }
}

#[test]
fn gui_config_writes_cannot_use_implicit_standalone_entry_points() {
    let sources = [
        include_str!("../src/app/provider_settings.rs"),
        include_str!("../src/app/role_profiles.rs"),
        include_str!("../src/app/role_settings.rs"),
        include_str!("../src/app/routing_settings.rs"),
        include_str!("../src/app/auto_grouping.rs"),
        include_str!("../src/app/project_dialog.rs"),
        include_str!("../src/app/sandbox_settings.rs"),
        include_str!("../src/model/self_improvement_settings.rs"),
    ];
    for source in sources {
        let production = source.split("#[cfg(test)]").next().unwrap();
        for forbidden in [
            "config::save_",
            "config::save::save_",
            "config::delete_",
            "StandaloneSaveContext",
            "file_overrides.insert",
        ] {
            assert!(
                !production.contains(forbidden),
                "GUI bypasses required save context: {forbidden}"
            );
        }
    }
    assert!(
        include_str!("../src/app/role_profiles.rs").contains("&config::LayeredSaveContext<'_>")
    );
}
