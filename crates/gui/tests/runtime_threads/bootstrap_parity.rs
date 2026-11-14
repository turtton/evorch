//! Compare actual core composition with the factories used by the legacy GUI.
//! Schema equality/order is intentional: it protects the provider cache prefix.
use gui::model::production::{ProductionModel, compose_production_model};
use runtime::{AgentModel, ModelSource, Role, RuntimeComposition, compose_runtime};
use std::{collections::BTreeMap, sync::Arc};
use workbench_host::bootstrap::{BootstrapOptions, BootstrappedHost, components};

struct EvaluationHost;
impl eval::evaluator::EvaluatorHost for EvaluationHost {
    fn deliver_notice(&self, _: &str, _: &str) -> Result<String, String> {
        unreachable!("no tools executed")
    }
    fn reload_config(&self) -> Result<(), String> {
        unreachable!("no tools executed")
    }
}

#[tokio::test]
async fn configured_core_and_gui_share_tools_models_providers_and_storage() {
    let dir = test_socket_dir::tempdir();
    let root = dir.path().canonicalize().unwrap();
    let user_dir = root.join("user");
    std::fs::create_dir(&user_dir).unwrap();
    // Only user configuration widens capabilities. The project may select routes,
    // but must not override trusted tool settings.
    std::fs::write(
        user_dir.join("config.toml"),
        r#"
[providers.local]
type = "anthropic-subscription"
api_protocol = "anthropic-messages"
base_url = "http://127.0.0.1:9/v1"
credential = { type = "keyring", service = "evorch", account = "p8a-parity-unused" }
models = ["parity-model"]
default_model = "parity-model"
[providers.secondary]
type = "openai-codex"
api_protocol = "openai-codex-responses"
base_url = "http://127.0.0.1:9/v1"
credential = { type = "keyring", service = "evorch", account = "p8a-parity-secondary-unused" }
models = ["secondary-model"]
default_model = "secondary-model"
[[routing.routes.explorer]]
profile = "secondary"
[[routing.routes.worker]]
profile = "local"
[tools.browser]
enabled = false
[tools.run_js]
enabled = true
scopes = ["worker:deep"]
max_tool_calls = 17
[sandbox]
web_tools_enabled = false
escalation_approval = "user"
escalate_to_user_on_deny = true
[metrics]
retention_days = 13
[diagnostics]
retain_terminal_task_progress = true
[storage.thread_index]
enabled = false
max_bytes = 2097152
"#,
    )
    .unwrap();
    let options = config::LoadOptions {
        user_config_dir: Some(user_dir.clone()),
        project_dir: Some(root.clone()),
        read_env: false,
        ..Default::default()
    };
    let config = config::Config::load(&options).unwrap();
    // Core now starts catalog as a host service; keep this composition test offline.
    let catalog_dir = user_dir.join("catalog");
    std::fs::create_dir_all(&catalog_dir).unwrap();
    std::fs::copy(
        test_support::model_catalog_dir().join("models-dev.json"),
        catalog_dir.join("models-dev.json"),
    )
    .unwrap();
    let mut core = BootstrappedHost::open(BootstrapOptions {
        config_dir: user_dir.clone(),
        project_root: root.clone(),
        ownership_root: root.join("owners"),
    })
    .await
    .unwrap();
    assert_eq!(core.config.providers, config.providers);
    let gui_storage_config = components::storage_config(&config, user_dir.join("evorch-events.db"));
    assert_eq!(core.composition.storage, gui_storage_config);
    assert_eq!(gui_storage_config.usage_retention_days, 13);
    assert!(!gui_storage_config.thread_index.enabled);
    assert_eq!(gui_storage_config.thread_index.max_bytes, 2097152);
    assert!(gui_storage_config.retain_terminal_task_progress);

    let bus = Arc::new(event_bus::EventBus::new(32));
    let eval_config = components::storage_config(&config, root.join("eval-test.db"));
    let eval_storage = storage::Storage::open(eval_config.clone()).unwrap();
    let (host_tools, _) = eval::evaluator::tools_with_monitor(eval::evaluator::EvaluatorContext {
        database: eval_config.db_path,
        storage: eval_storage.handle(),
        user_config_dir: user_dir.clone(),
        catalog_dir: Some(test_support::model_catalog_dir()),
        bus: bus.clone(),
        host: Arc::new(EvaluationHost),
        cli: None,
    });
    let extension_names = host_tools
        .iter()
        .map(|tool| tool.name().to_owned())
        .collect::<Vec<_>>();
    assert!(!extension_names.is_empty());
    let execution =
        components::ExecutionComponents::production(bus.clone(), root.clone(), &config, host_tools)
            .unwrap();
    let gui_tools = execution.executor.tool_specs();
    let common_tools = gui_tools
        .iter()
        .filter(|tool| !extension_names.contains(&tool.name))
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(core.composition.tools, gui_tools);
    assert_eq!(gui_tools.len(), common_tools.len() + extension_names.len());
    assert!(common_tools.iter().any(|tool| tool.name == "shell"));
    assert!(common_tools.iter().any(|tool| tool.name == "web_fetch")); // Exposure is gated per run.
    assert!(
        !common_tools
            .iter()
            .any(|tool| tool.name.starts_with("browser_"))
    );

    let credentials = Arc::new(
        sandbox::credential::FileCredentialStore::open(root.join("test-credentials")).unwrap(),
    );
    let model = compose_production_model(
        &config,
        &ProductionModel {
            load_options: options,
            credential_store: credentials.clone(),
            bus: bus.clone(),
            env: Arc::new(routing::MapEnv::default()),
            model_catalog_dir: Some(test_support::model_catalog_dir()),
        },
    )
    .unwrap();
    // Subscription credentials are resolved only on a request; construction
    // never needs process-global environment mutation or a real login.
    assert_eq!(
        model.selected_model(Role::Worker, None),
        "local/parity-model"
    );
    assert_eq!(
        model.selected_model(Role::Explorer, None),
        "secondary/secondary-model"
    );
    let profiles = model
        .available_profiles()
        .into_iter()
        .map(|profile| profile.name)
        .collect();
    let selected = [
        ("orchestrator", Role::Orchestrator),
        ("explorer", Role::Explorer),
        ("worker", Role::Worker),
        ("reviewer", Role::Reviewer),
    ]
    .into_iter()
    .map(|(name, role)| (name.to_owned(), model.selected_model(role, None)))
    .collect::<BTreeMap<_, _>>();
    assert_eq!(
        core.composition.model,
        runtime::compose::ModelIdentity::Routed { profiles, selected }
    );
    // Both roots preserve the same routed selection inside a live-reload switch.
    let gui = compose_runtime(RuntimeComposition {
        config: &config,
        user_config_dir: Some(user_dir.clone()),
        model_catalog_dir: Some(test_support::model_catalog_dir()),
        bus,
        executor: execution.executor,
        credential_store: credentials,
        env: Arc::new(routing::MapEnv::default()),
        model_source: ModelSource::Fixed(Arc::new(runtime::compose::SwitchableModel::new(model))),
        workspace: Some(execution.workspace),
    })
    .unwrap();
    assert_eq!(gui.model_identity, runtime::compose::ModelIdentity::Fixed);
    assert_eq!(
        core.runtime.run_js_settings(),
        gui.runtime.run_js_settings()
    );
    assert_eq!(core.runtime.run_js_settings(), config.tools.run_js);
    assert!(!core.runtime.web_tools_enabled());
    assert_eq!(
        core.runtime.web_tools_enabled(),
        gui.runtime.web_tools_enabled()
    );
    for role in [
        Role::Orchestrator,
        Role::Explorer,
        Role::Worker,
        Role::Reviewer,
    ] {
        let policy = core.runtime.execution_policy(role);
        assert_eq!(policy, gui.runtime.execution_policy(role));
        assert_eq!(policy.escalation_approval, config::EscalationApproval::User);
        assert!(policy.escalate_to_user_on_deny);
    }
    // PR03c intentionally starts these services at the core composition root.
    // Additional host services do not affect the shared constructor parity.
    let services = core.services.names().collect::<Vec<_>>();
    assert!(services.contains(&"mcp"));
    assert!(services.contains(&"catalog"));
    // Neither root infers project trust or extra mounts from the project config.
    assert!(
        core.runtime
            .project_allowed_directories(&root)
            .get()
            .is_empty()
    );
    assert!(
        gui.runtime
            .project_allowed_directories(&root)
            .get()
            .is_empty()
    );
    core.shutdown().await.unwrap();

    // Exercise real GUI approval and its serialized sidebar, not a hand-written
    // projection. Both paths must grant the same access and project rules.
    let allowed = tempfile::tempdir().unwrap();
    let allowed_path = allowed.path().canonicalize().unwrap();
    let marker = "p8a-approved-project-rule";
    std::fs::write(root.join("AGENTS.md"), marker).unwrap();
    let project = workspace_ui::ProjectId::new("approved-project");
    let mut sidebar = workspace_ui::SidebarState::default();
    sidebar
        .add_project(project.clone(), "Approved project", &root)
        .unwrap();
    let sink = gui::runtime_sink::RuntimeCommandSink::new(
        gui.runtime.clone(),
        tokio::runtime::Handle::current(),
    );
    let mut state = gui::app::WorkbenchState::new(
        gui::fixture::DemoSource(Vec::new()),
        &workspace_ui::UiSettings::default(),
    )
    .unwrap()
    .with_sidebar(sidebar)
    .with_command_sink(Box::new(sink));
    state
        .add_allowed_directory(&project, &allowed_path)
        .unwrap();
    state
        .set_project_trust(&project, workspace_ui::TrustState::Approved)
        .unwrap();
    std::fs::write(
        user_dir.join("sidebar.json"),
        serde_json::to_vec(state.sidebar()).unwrap(),
    )
    .unwrap();
    let mut approved_core = BootstrappedHost::open(BootstrapOptions {
        config_dir: user_dir,
        project_root: root.clone(),
        ownership_root: root.join("owners"),
    })
    .await
    .unwrap();
    for runtime in [&approved_core.runtime, &gui.runtime] {
        assert_eq!(
            runtime.project_allowed_directories(&root).get().as_ref(),
            std::slice::from_ref(&allowed_path)
        );
        let mut request = runtime::base_context::BaseContextRequest::new(Role::Worker);
        request.project_root = Some(root.clone());
        let report = runtime.preview_base_context(request).await.unwrap();
        assert!(report.sections.iter().any(|section| {
            section.kind == runtime::base_context::ContextSectionKind::Rules
                && section.text.contains(marker)
        }));
    }
    approved_core.shutdown().await.unwrap();
    drop(gui);
    drop(eval_storage);
}

#[tokio::test]
async fn unconfigured_core_is_read_only_while_gui_demo_keeps_standard_tools() {
    let dir = test_socket_dir::tempdir();
    let root = dir.path().canonicalize().unwrap();
    let mut core = BootstrappedHost::open(BootstrapOptions {
        config_dir: root.clone(),
        project_root: root.clone(),
        ownership_root: root.join("owners"),
    })
    .await
    .unwrap();
    assert!(core.composition.tools.is_empty());
    assert_eq!(
        core.composition.model,
        runtime::compose::ModelIdentity::Fixed
    );
    let mut config = config::Config::default();
    config.tools.browser.enabled = false;
    let gui = components::ExecutionComponents::production(
        Arc::new(event_bus::EventBus::new(16)),
        root,
        &config,
        Vec::new(),
    )
    .unwrap();
    assert!(
        gui.executor
            .tool_specs()
            .iter()
            .any(|tool| tool.name == "write")
    );
    core.shutdown().await.unwrap();
}
