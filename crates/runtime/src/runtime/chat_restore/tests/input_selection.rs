use super::*;
use crate::{ModelPreference, ModelPreferenceUpdate};
use providers::{ChatRequest, ChatResponse, Message, ToolSpec};
use std::collections::BTreeMap;
use tokio::sync::{Notify, Semaphore, mpsc};

struct SelectionModel {
    requests: mpsc::UnboundedSender<crate::quiesce::ProviderInput>,
    inputs: Mutex<BTreeMap<String, crate::quiesce::ProviderInput>>,
    release: Semaphore,
    hold_preparation: AtomicBool,
    preparing: Notify,
    prepare_release: Semaphore,
}

impl SelectionModel {
    fn new() -> (
        Arc<Self>,
        mpsc::UnboundedReceiver<crate::quiesce::ProviderInput>,
    ) {
        let (requests, received) = mpsc::unbounded_channel();
        (
            Arc::new(Self {
                requests,
                inputs: Default::default(),
                release: Semaphore::new(0),
                hold_preparation: AtomicBool::new(false),
                preparing: Notify::new(),
                prepare_release: Semaphore::new(0),
            }),
            received,
        )
    }
}

#[async_trait::async_trait]
impl AgentModel for SelectionModel {
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "p/default".into()
    }
    fn validate_model_preference(&self, p: &ModelPreference) -> Result<(), RuntimeError> {
        assert_eq!(p.profile, "p");
        Ok(())
    }
    fn validate_checkpoint_compatibility(
        &self,
        invocation: &crate::AgentInvocationContext,
        _: Role,
        _: &[crate::RoutePinRecord],
        input: Option<&crate::quiesce::ProviderInput>,
    ) -> Result<(), RuntimeError> {
        let expected = invocation
            .model_preference
            .as_ref()
            .and_then(|p| p.model.as_deref())
            .unwrap_or("default");
        assert_eq!(input.unwrap().request.model, expected);
        Ok(())
    }
    fn checkpoint_provider_input(&self, run: &str) -> Option<crate::quiesce::ProviderInput> {
        self.inputs.lock().unwrap().get(run).cloned()
    }
    async fn prepare_replay_context(
        &self,
        invocation: &crate::AgentInvocationContext,
        _: Role,
        _: &[Message],
        _: &[ToolSpec],
        _: &crate::ResponsesReplayState,
    ) -> Result<(), RuntimeError> {
        if invocation.model_preference.is_some()
            && self.hold_preparation.swap(false, Ordering::SeqCst)
        {
            self.preparing.notify_one();
            self.prepare_release.acquire().await.unwrap().forget();
        }
        Ok(())
    }
    async fn complete(
        &self,
        invocation: &crate::AgentInvocationContext,
        role: Role,
        messages: &[Message],
        tools: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        let input = crate::quiesce::ProviderInput {
            profile: "p".into(),
            protocol: model::ApiProtocol::OpenAiCompletions,
            edit_tool: config::EditToolConfig::Edit,
            affinity_key: invocation.run_id.clone(),
            replay: None,
            request: ChatRequest {
                model: invocation
                    .model_preference
                    .as_ref()
                    .and_then(|p| p.model.clone())
                    .unwrap_or_else(|| "default".into()),
                reasoning_effort: invocation
                    .model_preference
                    .as_ref()
                    .and_then(|p| p.reasoning_effort.clone()),
                messages: messages.to_vec(),
                tools: tools.to_vec(),
                tool_choice: None,
                temperature: None,
                max_tokens: None,
                service_tier: None,
                output_schema: None,
                observation: None,
                responses_replay: None,
            },
        };
        self.inputs
            .lock()
            .unwrap()
            .insert(invocation.run_id.clone(), input.clone());
        self.requests.send(input).unwrap();
        self.release.acquire().await.unwrap().forget();
        CompletingModel
            .complete(invocation, role, messages, tools)
            .await
    }
}

fn descriptor(runtime: &AgentRuntime, run: RunId) -> RunRestoreDescriptor {
    let record = runtime
        .shared
        .run_store
        .get()
        .unwrap()
        .restore_record(run)
        .unwrap()
        .unwrap();
    serde_json::from_str(&record.config_json).unwrap()
}

fn explicit() -> ModelPreferenceUpdate {
    ModelPreferenceUpdate::explicit(ModelPreference {
        profile: "p".into(),
        model: Some("chosen".into()),
        reasoning_effort: Some("custom".into()),
    })
}

async fn waiting(runtime: &AgentRuntime, run: RunId) {
    let mut phase = runtime.entry(run).unwrap().phase_rx.clone();
    loop {
        let current = *phase.borrow_and_update();
        if current == AgentRunPhase::Waiting {
            return;
        }
        assert!(!matches!(
            current,
            AgentRunPhase::Error | AgentRunPhase::Stopped | AgentRunPhase::Done
        ));
        phase.changed().await.unwrap();
    }
}

#[tokio::test]
async fn queued_and_withdrawn_choices_do_not_change_the_inflight_request() {
    let (model, mut requests) = SelectionModel::new();
    let f = Fixture::with_model(model.clone());
    let run = f.runtime.delegate_background(
        Role::Worker,
        "start".into(),
        RunConfig {
            interactive: true,
            keep_alive: true,
            ..Default::default()
        },
    );
    assert_eq!(requests.recv().await.unwrap().request.model, "default");
    f.runtime
        .continue_goal_with_model_update(
            run,
            "withdrawn".into(),
            RunConfig {
                initial_prompt_id: Some("withdrawn".into()),
                ..Default::default()
            },
            explicit(),
        )
        .unwrap();
    assert_eq!(
        *f.runtime.entry(run).unwrap().model_preference_tx.borrow(),
        None
    );
    f.runtime.withdraw_follow_up(run, "withdrawn").unwrap();
    model.release.add_permits(1);
    waiting(&f.runtime, run).await;
    assert_eq!(
        *f.runtime.entry(run).unwrap().model_preference_tx.borrow(),
        None
    );
    f.runtime
        .continue_goal_with_model_update(run, "apply".into(), RunConfig::default(), explicit())
        .unwrap();
    let request = requests.recv().await.unwrap();
    assert_eq!(request.request.model, "chosen");
    assert_eq!(request.request.reasoning_effort.as_deref(), Some("custom"));
    f.runtime
        .continue_goal(run, "keep".into(), RunConfig::default())
        .unwrap();
    model.release.add_permits(1);
    assert_eq!(requests.recv().await.unwrap().request.model, "chosen");
    f.runtime.cancel(run).unwrap();
    f.runtime.wait(run).await.unwrap();
}

fn process(
    config: &storage::StorageConfig,
    storage: &storage::Storage,
    model: Arc<SelectionModel>,
) -> AgentRuntime {
    let bus = Arc::new(EventBus::new(256));
    let runtime = AgentRuntime::new(bus.clone(), Arc::new(ToolExecutor::new(bus)), model)
        .with_run_store(crate::RunStore::open(config, storage.handle()).unwrap());
    runtime.install_planned_recovery_fixture().unwrap();
    runtime
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn adopted_selection_restores_before_and_after_switch_preparation() {
    for prepared in [false, true] {
        let dir = test_socket_dir::SplitDir::new();
        let config = storage::StorageConfig {
            db_path: dir.path().join("selection.db"),
            ..Default::default()
        };
        let storage = storage::Storage::open(config.clone()).unwrap();
        let (model, mut requests) = SelectionModel::new();
        let runtime = process(&config, &storage, model.clone());
        let run = runtime.delegate_background(
            Role::Worker,
            "original".into(),
            RunConfig {
                interactive: true,
                keep_alive: true,
                ..Default::default()
            },
        );
        let before = requests.recv().await.unwrap();
        model.release.add_permits(1);
        waiting(&runtime, run).await;
        let (reached, held) = tokio::sync::oneshot::channel();
        let (release, released) = std::sync::mpsc::channel();
        if prepared {
            model.hold_preparation.store(true, Ordering::SeqCst);
        } else {
            runtime
                .entry(run)
                .unwrap()
                .user_inbox
                .gate_next_adoption(reached, released);
        }
        runtime
            .continue_goal_with_model_update(
                run,
                "new choice".into(),
                RunConfig::default(),
                explicit(),
            )
            .unwrap();
        if prepared {
            model.preparing.notified().await;
        } else {
            held.await.unwrap();
        }
        runtime.request_quiesce();
        if prepared {
            model.prepare_release.add_permits(1);
        } else {
            release.send(()).unwrap();
        }
        runtime.wait_quiesced().await.unwrap();
        let saved_descriptor = descriptor(&runtime, run);
        assert_eq!(
            saved_descriptor
                .input_selection_transition
                .as_ref()
                .unwrap()
                .prepared,
            prepared
        );
        let fence = runtime.fence_dispatch_restart().await.unwrap();
        runtime.drain_dispatch_tails(&fence).await.unwrap();
        let records = runtime
            .capture_recovery_records(
                &fence,
                &BTreeMap::from([(
                    run,
                    crate::quiesce::RecoveryRecipe {
                        settings_reference: crate::quiesce::CORE_RECOVERY_SETTINGS_REFERENCE.into(),
                        policy: crate::quiesce::ResumePolicy::CoreOwned,
                        descriptor: None,
                        goal_id: None,
                        team: None,
                        lane: None,
                    },
                )]),
            )
            .unwrap();
        runtime.save_recovery_graph("selection", &records).unwrap();
        drop(runtime);
        storage.close();
        let storage = storage::Storage::open(config.clone()).unwrap();
        let runtime = process(&config, &storage, model.clone());
        let ownership = dir.sockets().join("current");
        let leases =
            storage::shared_with_wait(&config.db_path, &ownership, std::time::Duration::ZERO)
                .unwrap();
        let startup = leases
            .recovery_startup_complete(&config.db_path, &ownership)
            .unwrap();
        let plan = runtime.recovery_plan().unwrap();
        let mut paused = runtime
            .prepare_component(&plan.components[0], startup, &ownership)
            .unwrap();
        runtime
            .restore_relations_and_continuations(&mut paused)
            .unwrap();
        let owner = crate::ownership::OwnerHost::open(
            &ownership,
            Default::default(),
            Arc::new(EventBus::new(64)),
        )
        .unwrap();
        let authority = crate::CurrentRecoveryAuthority {
            members: BTreeMap::from([(
                run,
                RunConfig {
                    interactive: true,
                    keep_alive: true,
                    model_preference: explicit().selection().flatten(),
                    ownership: Some(owner.start("current").unwrap()),
                    ..Default::default()
                },
            )]),
        };
        let reservation = runtime
            .reserve_automatic_activation(&paused.claim().manifest_id)
            .unwrap();
        runtime
            .activate_component(paused, authority, reservation)
            .unwrap()
            .1
            .release();
        let after = requests.recv().await.unwrap();
        assert_eq!(after.request.model, "chosen");
        assert_eq!(after.request.reasoning_effort.as_deref(), Some("custom"));
        assert!(after.request.messages.starts_with(&before.request.messages));
        assert_eq!(after.affinity_key, before.affinity_key);
        model.release.add_permits(1);
        waiting(&runtime, run).await;
        assert!(
            descriptor(&runtime, run)
                .input_selection_transition
                .is_none()
        );
        runtime.cancel(run).unwrap();
        runtime.wait(run).await.unwrap();
    }
}
