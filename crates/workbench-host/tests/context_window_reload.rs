//! Exercise the production settings reload, replacing only provider responses.
//! Compare a new run after reload with a runtime composed from the saved config.
use std::sync::Arc;

use event_bus::{AgentRunPhase, CompactionEvent, EventKind, LifecycleEvent};
use runtime::compose::SwitchableModel;
use runtime::{AgentModel, AgentRuntime, Role, RunConfig, RunId};
use workbench_host::bootstrap::models::{ProductionModel, compose_production_model, reload_models};

const MODEL: &str = "claude-opus-5-5";

/// Keep the real, reloadable model metadata, but never call a remote provider.
struct Responses(Arc<SwitchableModel>);

#[async_trait::async_trait]
impl AgentModel for Responses {
    fn selected_model(&self, role: Role, category: Option<&str>) -> String {
        self.0.selected_model(role, category)
    }

    fn catalog_context_window(&self, selected: &str) -> Option<u64> {
        self.0.catalog_context_window(selected)
    }

    async fn complete(
        &self,
        _: &runtime::AgentInvocationContext,
        _: Role,
        messages: &[providers::Message],
        _: &[providers::ToolSpec],
    ) -> Result<providers::ChatResponse, runtime::RuntimeError> {
        let has_answer = messages
            .iter()
            .any(|message| message.role == providers::Role::Assistant);
        Ok(providers::ChatResponse {
            message: providers::Message {
                role: providers::Role::Assistant,
                content: vec![providers::ContentBlock::Text {
                    text: if has_answer {
                        "done".into()
                    } else {
                        "history ".repeat(20_000)
                    },
                }],
            },
            finish_reason: providers::FinishReason::Stop,
            usage_reported: true,
            // Drive both automatic thresholds with provider-reported context,
            // without exceeding the 256 KiB assistant-reference storage limit.
            usage: providers::Usage {
                context_tokens: Some(if has_answer { 1_000 } else { 320_000 }),
                ..Default::default()
            },
        })
    }
}

struct Fixture {
    _directory: tempfile::TempDir,
    context: ProductionModel,
    storage: storage::Storage,
    storage_config: storage::StorageConfig,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let user = directory.path().join("user");
        std::fs::create_dir_all(&user).unwrap();
        let catalog = directory.path().join("catalog");
        std::fs::create_dir_all(&catalog).unwrap();
        // A pinned empty catalog prevents the network or models.dev from supplying
        // this model's window. The only variable is the saved override.
        std::fs::write(
            catalog.join("models-dev.json"),
            r#"{"fetched_at":4102444800,"api":{}}"#,
        )
        .unwrap();
        let context = ProductionModel {
            load_options: config::LoadOptions {
                user_config_dir: Some(user),
                read_env: false,
                ..Default::default()
            },
            credential_store: Arc::new(
                sandbox::credential::FileCredentialStore::open(
                    directory.path().join("credentials"),
                )
                .unwrap(),
            ),
            bus: Arc::new(event_bus::EventBus::new(1024)),
            env: Arc::new(routing::MapEnv::from_iter([(
                "ANTHROPIC_API_KEY",
                "fixture-not-a-real-key",
            )])),
            model_catalog_dir: Some(catalog),
        };
        let storage_config = storage::StorageConfig {
            db_path: directory.path().join("runs.db"),
            ..Default::default()
        };
        let storage = storage::Storage::open(storage_config.clone()).unwrap();
        Self {
            _directory: directory,
            context,
            storage,
            storage_config,
        }
    }

    fn save(&self, window: u64, model_override: bool) -> config::Config {
        let (default_window, entry_window) = if model_override {
            (
                200_000,
                format!(", metadata_source = 'manual', context_window = {window}"),
            )
        } else {
            (window, String::new())
        };
        std::fs::write(
            self.context
                .load_options
                .user_config_dir
                .as_ref()
                .unwrap()
                .join("config.toml"),
            format!(
                "[providers.local]\n\
                 provider_type = 'anthropic'\n\
                 api_protocol = 'anthropic-messages'\n\
                 default_model = '{MODEL}'\n\
                 models = [{{ id = '{MODEL}', enabled = true{entry_window} }}]\n\
                 [routing.routes]\n\
                 worker = [{{ profile = 'local', model = '{MODEL}', auto = false }}]\n\
                 [compaction]\n\
                 context_window_tokens = {default_window}\n\
                 threshold = 0.75\n\
                 keep_recent_tokens = 1\n\
                 summarizer = 'structural'\n"
            ),
        )
        .unwrap();
        config::Config::load(&self.context.load_options).unwrap()
    }

    fn runtime(&self, config: &config::Config, model: Arc<SwitchableModel>) -> AgentRuntime {
        runtime::compose_runtime(runtime::RuntimeComposition {
            config,
            bus: self.context.bus.clone(),
            executor: Arc::new(tools::ToolExecutor::new(self.context.bus.clone())),
            credential_store: self.context.credential_store.clone(),
            env: self.context.env.clone(),
            model_source: runtime::ModelSource::Fixed(Arc::new(Responses(model))),
            workspace: None,
            user_config_dir: self.context.load_options.user_config_dir.clone(),
            model_catalog_dir: self.context.model_catalog_dir.clone(),
        })
        .unwrap()
        .runtime
        .with_run_store(
            runtime::RunStore::open(&self.storage_config, self.storage.handle()).unwrap(),
        )
    }
}

async fn wait_for_turn(events: &mut event_bus::EventReceiver, run: RunId) -> Vec<CompactionEvent> {
    let mut compacted = Vec::new();
    loop {
        match events.recv().await.unwrap().kind {
            EventKind::Compaction(event @ CompactionEvent::Compacted { .. }) => {
                if let CompactionEvent::Compacted { run_id, .. } = &event
                    && run_id == &run.to_string()
                {
                    compacted.push(event);
                }
            }
            EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
                run_id,
                to,
                reason,
                ..
            }) if run_id == run.to_string() => match to {
                AgentRunPhase::Waiting => return compacted,
                AgentRunPhase::Done | AgentRunPhase::Error | AgentRunPhase::Stopped => {
                    panic!("run {run} ended before its turn completed: {to:?}: {reason:?}");
                }
                _ => {}
            },
            _ => {}
        }
    }
}

async fn start(runtime: &AgentRuntime) -> RunId {
    let mut events = runtime.event_bus().subscribe();
    let run = runtime.delegate_background(
        Role::Worker,
        "retain the original goal".into(),
        RunConfig {
            interactive: true,
            keep_alive: true,
            ..Default::default()
        },
    );
    assert!(wait_for_turn(&mut events, run).await.is_empty());
    run
}

async fn automatic_compaction(runtime: &AgentRuntime, run: RunId) -> (u64, f64) {
    let mut events = runtime.event_bus().subscribe();
    runtime.send_message(run, "continue".into()).unwrap();
    let events = wait_for_turn(&mut events, run).await;
    runtime.stop(run, runtime::StopScope::SelfOnly).unwrap();
    runtime.wait(run).await.unwrap();
    assert_eq!(
        events.len(),
        1,
        "one automatic compaction committed: {events:?}"
    );
    let CompactionEvent::Compacted {
        reason,
        context_window_tokens,
        threshold,
        estimated_tokens_before,
        ..
    } = &events[0]
    else {
        unreachable!()
    };
    assert_eq!(*reason, event_bus::CompactionReason::Automatic);
    assert!(*estimated_tokens_before >= 300_000);
    (*context_window_tokens, *threshold)
}

async fn reload_scenario(initial_model_override: bool, model_override: bool) {
    let fixture = Fixture::new();
    let initial = fixture.save(200_000, initial_model_override);
    let model = Arc::new(SwitchableModel::new(
        compose_production_model(&initial, &fixture.context).unwrap(),
    ));
    assert_eq!(
        model.selected_model(Role::Worker, None),
        format!("local/{MODEL}")
    );
    let runtime = fixture.runtime(&initial, model.clone());
    // Also warm ModelResolution and keep a run active across the reload.
    let existing = start(&runtime).await;

    let saved = fixture.save(400_000, model_override);
    if model_override {
        assert_eq!(
            saved.providers["local"].models[0].context_window,
            Some(400_000)
        );
    } else {
        assert_eq!(saved.compaction.context_window_tokens, 400_000);
    }
    let previous = model.recovery_snapshot().unwrap().generation;
    reload_models(
        Some(&(fixture.context.clone(), model.clone())),
        None,
        Some(&runtime),
    )
    .unwrap();
    assert!(!Arc::ptr_eq(
        &previous,
        &model.recovery_snapshot().unwrap().generation
    ));
    let new_run = start(&runtime).await;
    let existing_settings = automatic_compaction(&runtime, existing).await;
    let reloaded_settings = automatic_compaction(&runtime, new_run).await;

    // Cold-start control: the very same saved config works after reconstruction.
    let fresh_model = Arc::new(SwitchableModel::new(
        compose_production_model(&saved, &fixture.context).unwrap(),
    ));
    assert_eq!(
        fresh_model.selected_model(Role::Worker, None),
        format!("local/{MODEL}")
    );
    let restarted = fixture.runtime(&saved, fresh_model);
    let restarted_run = start(&restarted).await;
    let restarted_settings = automatic_compaction(&restarted, restarted_run).await;
    assert_eq!(
        existing_settings,
        (200_000, 0.75),
        "active run must stay pinned"
    );
    assert_eq!(restarted_settings, (400_000, 0.75), "cold-start control");
    assert_eq!(
        reloaded_settings, restarted_settings,
        "a new run after settings reload must use the saved 400K window (300K trigger), not the old 200K window (150K trigger)"
    );
}

#[tokio::test]
async fn new_run_after_model_context_override_reload_uses_saved_window() {
    reload_scenario(true, true).await;
}

#[tokio::test]
async fn new_run_after_adding_model_context_override_uses_saved_window() {
    reload_scenario(false, true).await;
}

#[tokio::test]
async fn new_run_after_compaction_default_reload_uses_saved_window() {
    reload_scenario(false, false).await;
}
