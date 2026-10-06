//! Settings save into the user config and reload every layer; projects only select a role profile.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gui::model::provider_settings::{CredentialMode, ProviderKind};
use gui::{app::WorkbenchState, fixture::DemoSource, headless::HeadlessWorkbench};

fn write_config(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().expect("config parent")).expect("config directory");
    std::fs::write(path, text).expect("config fixture");
}

fn fixture(
    root: &Path,
    production: bool,
) -> (WorkbenchState<DemoSource>, config::LoadOptions, PathBuf) {
    let project = root.join("project");
    let user = root.join("user");
    // Projects may only select a role profile: these keys are ignored on load.
    write_config(
        &config::project_main_config_path(&project),
        r#"[providers.ignored]
type = "openai-compatible"
base_url = "https://example.invalid/v1"
api_key_env = "TEST_KEY"
models = ["base"]
default_model = "base"
[agents.worker]
preset = "project-ignored"
"#,
    );
    write_config(
        &user.join("config.toml"),
        r#"[providers.project]
type = "openai-compatible"
base_url = "https://example.invalid/v1"
api_key_env = "TEST_KEY"
models = ["base"]
default_model = "base"
[agents.explorer]
preset = "user-layer"
"#,
    );
    // A drop-in proves reloads read the whole user layer, not only the save path.
    write_config(
        &user.join("config.d/extra.toml"),
        "[agents.reviewer]\npreset = 'user-dropin'\n",
    );
    let options = config::LoadOptions {
        project_dir: Some(project),
        user_config_dir: Some(user.clone()),
        read_env: false,
        ..Default::default()
    };
    let save_path = user.join("config.toml");
    let mut state =
        WorkbenchState::new(DemoSource(Vec::new()), &workspace_ui::UiSettings::default())
            .expect("state")
            .with_settings_load_options(if production {
                // Production must use its context, not even these explicit fallback layers.
                config::LoadOptions {
                    user_config_dir: Some(root.join("unused-user")),
                    read_env: false,
                    ..Default::default()
                }
            } else {
                options.clone()
            })
            .with_provider_settings_path(save_path.clone());
    if production {
        let context = gui::model::production::ProductionModel {
            load_options: options.clone(),
            credential_store: Arc::new(
                sandbox::FileCredentialStore::open(root.join("credentials")).expect("store"),
            ),
            bus: Arc::new(event_bus::EventBus::new(32)),
            env: Arc::new(routing::MapEnv::from_iter([("TEST_KEY", "test-secret")])),
        };
        let model = Arc::new(runtime::compose::SwitchableModel::new(
            context.reload().expect("production model"),
        ));
        state = state.with_production_model(context, model);
    }
    (state, options, save_path)
}

fn assert_user_layer_agents(agents: &config::AgentsConfig) {
    assert_eq!(agents.explorer.preset.as_deref(), Some("user-layer"));
    assert_eq!(agents.reviewer.preset.as_deref(), Some("user-dropin"));
    assert_eq!(agents.worker.preset, None);
}

fn finish(harness: &mut HeadlessWorkbench<DemoSource>) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while harness.state().role_settings().is_saving()
        || harness.state().routing_settings().is_saving()
    {
        assert!(Instant::now() < deadline, "settings save timed out");
        harness.step();
        std::thread::yield_now();
    }
}

#[test]
fn provider_save_reloads_user_layers_and_ignores_project_settings() {
    for production in [false, true] {
        let temp = tempfile::tempdir().expect("temp");
        let (mut state, options, save_path) = fixture(temp.path(), production);
        state.open_provider_settings();
        state
            .provider_settings_mut()
            .add(ProviderKind::OpenAiCompatible);
        let editor = state.provider_settings_mut().openai_mut().expect("editor");
        editor.name = "added".into();
        editor.base_url = "https://example.invalid/v1".into();
        editor.credential_mode = CredentialMode::Env;
        editor.api_key_env = "TEST_KEY".into();
        editor.models = vec![config::ModelEntryConfig::enabled("base")];
        editor.default_model = "base".into();
        state.submit_provider_settings();
        let mut harness = HeadlessWorkbench::new(state, [1200.0, 900.0]);
        harness.wait_provider_save(Duration::from_secs(10));
        let settings = harness.state().provider_settings();
        assert_eq!(settings.error, None);
        assert!(settings.editor.is_none());
        let names: Vec<_> = settings
            .profiles
            .iter()
            .map(|profile| profile.name.as_str())
            .collect();
        assert_eq!(names, ["added", "project"]);
        assert!(
            std::fs::read_to_string(save_path)
                .expect("saved")
                .contains("providers.added")
        );
        assert_eq!(
            config::Config::load(&options)
                .expect("reload")
                .providers
                .len(),
            2
        );
        harness.state_mut().open_role_settings();
        assert_user_layer_agents(&harness.state().role_settings().agents);
    }
}

#[test]
fn role_save_and_route_rename_write_the_user_target() {
    for production in [false, true] {
        let temp = tempfile::tempdir().expect("temp");
        let (mut state, options, save_path) = fixture(temp.path(), production);
        let routing = config::RoutingConfig {
            routes: [(
                "old".into(),
                vec![config::RouteCandidateConfig {
                    profile: "project".into(),
                    model: None,
                    reasoning_effort: None,
                }],
            )]
            .into(),
        };
        config::save_routing(&save_path, &routing).expect("route fixture");
        state.open_role_settings();
        assert_eq!(state.role_settings().error, None);
        state.role_settings_mut().agents.worker.base.logical_model = Some("old".into());
        state.submit_role_settings();
        let mut harness = HeadlessWorkbench::new(state, [1200.0, 900.0]);
        finish(&mut harness);
        assert_eq!(harness.state().role_settings().error, None);
        assert_eq!(
            harness
                .state()
                .role_settings()
                .agents
                .worker
                .logical_model
                .as_deref(),
            Some("old")
        );
        harness.state_mut().open_routing_settings();
        harness
            .state_mut()
            .routing_settings_mut()
            .rename_route("old", "new")
            .expect("rename");
        harness.state_mut().submit_routing_settings();
        finish(&mut harness);
        assert_eq!(harness.state().routing_settings().validation_error, None);
        assert!(
            harness
                .state()
                .routing_settings()
                .routes
                .contains_key("new")
        );
        assert!(
            !harness
                .state()
                .routing_settings()
                .routes
                .contains_key("old")
        );
        let saved = config::Config::load(&options).expect("reload");
        assert_eq!(saved.agents.worker.logical_model.as_deref(), Some("new"));
        assert_user_layer_agents(&saved.agents);
    }
}

#[test]
fn user_route_rename_is_blocked_by_user_dropin_without_writing() {
    let temp = tempfile::tempdir().expect("temp");
    let (mut state, options, save_path) = fixture(temp.path(), true);
    let original = format!(
        "{}[routing.routes]\nold = [{{profile = 'project'}}]\n[agents.worker]\nlogical_model = 'old'\n",
        std::fs::read_to_string(&save_path).expect("user config")
    );
    write_config(&save_path, &original);
    write_config(
        &options
            .user_config_dir
            .as_ref()
            .expect("user")
            .join("config.d/pinned.toml"),
        "[agents.worker]\nlogical_model = 'old'\n",
    );
    state.open_routing_settings();
    state
        .routing_settings_mut()
        .rename_route("old", "new")
        .expect("rename");
    state.submit_routing_settings();
    let mut harness = HeadlessWorkbench::new(state, [1200.0, 900.0]);
    finish(&mut harness);
    let error = harness
        .state()
        .routing_settings()
        .validation_error
        .as_deref()
        .expect("blocked rename");
    assert!(error.contains("Route rename blocked"), "{error}");
    assert_eq!(
        std::fs::read_to_string(save_path).expect("unchanged"),
        original
    );
}

#[test]
fn all_settings_saves_create_missing_parent_directories() {
    for kind in [
        "openai",
        "codex",
        "role",
        "routing",
        "rename",
        "sandbox",
        "self-improvement",
    ] {
        let temp = tempfile::tempdir().expect("temp");
        let project = temp.path().join("project");
        let user = temp.path().join("user");
        let path = user.join("config.toml");
        let options = config::LoadOptions {
            project_dir: Some(project),
            user_config_dir: Some(user),
            read_env: false,
            ..Default::default()
        };
        let mut state =
            WorkbenchState::new(DemoSource(Vec::new()), &workspace_ui::UiSettings::default())
                .expect("state")
                .with_settings_load_options(options.clone())
                .with_provider_settings_path(path.clone());
        assert!(!path.parent().expect("parent").exists());
        match kind {
            "openai" | "codex" => {
                state.open_provider_settings();
                state.provider_settings_mut().add(if kind == "openai" {
                    ProviderKind::OpenAiCompatible
                } else {
                    ProviderKind::CodexSubscription
                });
                if let Some(editor) = state.provider_settings_mut().openai_mut() {
                    editor.base_url = "https://example.invalid/v1".into();
                    editor.credential_mode = CredentialMode::Env;
                    editor.api_key_env = "TEST_KEY".into();
                    editor.models = vec![config::ModelEntryConfig::enabled("base")];
                    editor.default_model = "base".into();
                }
                state.submit_provider_settings();
            }
            "role" => {
                state.open_role_settings();
                state.submit_role_settings();
            }
            "routing" | "rename" => {
                state.open_routing_settings();
                if kind == "rename" {
                    let model = state.routing_settings_mut();
                    model.profile_names.push("test".into());
                    model.add_route("old").expect("route");
                    model.rename_route("old", "new").expect("rename");
                }
                state.submit_routing_settings();
            }
            "sandbox" => state.open_sandbox_settings(),
            "self-improvement" => state.open_self_improvement_settings(),
            _ => unreachable!(),
        }
        let mut harness = HeadlessWorkbench::new(state, [1200.0, 900.0]);
        if matches!(kind, "sandbox" | "self-improvement") {
            harness.run();
            harness.click_label(if kind == "sandbox" {
                "Save sandbox"
            } else {
                "Save self-improvement"
            });
        }
        harness.wait_provider_save(Duration::from_secs(10));
        finish(&mut harness);
        let deadline = Instant::now() + Duration::from_secs(10);
        while !path.is_file() {
            assert!(Instant::now() < deadline, "{kind}: save timed out");
            harness.step();
            std::thread::yield_now();
        }
        assert_eq!(harness.state().provider_settings().error, None);
        assert_eq!(harness.state().role_settings().error, None);
        assert_eq!(harness.state().routing_settings().validation_error, None);
        config::Config::load(&options).expect("saved settings reload");
    }
}
