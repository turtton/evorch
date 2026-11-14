use super::*;
use crate::{AgentInvocationContext, AgentModel, AgentRuntime, Role, RunConfig, RuntimeError};
use event_bus::{AgentRunPhase, EventBus, EventKind, LifecycleEvent};
use providers::{ChatRequest, ChatResponse, ContentBlock, FinishReason, Message, ToolSpec};
use serde_json::json;
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::sync::{Notify, Semaphore};

struct WaitModel {
    targets: Mutex<Option<(RunId, RunId)>>,
    requested_wait: AtomicBool,
    child_release: Semaphore,
    child_entered: Notify,
    parent_resumed: Notify,
    inputs: Mutex<BTreeMap<String, Vec<ProviderInput>>>,
}
#[async_trait::async_trait]
impl AgentModel for WaitModel {
    fn validate_checkpoint_compatibility(
        &self,
        invocation: &AgentInvocationContext,
        _: Role,
        pins: &[crate::RoutePinRecord],
        input: Option<&ProviderInput>,
    ) -> Result<(), RuntimeError> {
        assert!(pins.is_empty());
        assert!(
            input.is_none_or(|i| i.request.model == "test" && i.affinity_key == invocation.run_id)
        );
        Ok(())
    }
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "test".into()
    }
    fn checkpoint_provider_input(&self, key: &str) -> Option<ProviderInput> {
        self.inputs.lock().unwrap().get(key)?.last().cloned()
    }
    async fn complete(
        &self,
        invocation: &AgentInvocationContext,
        _: Role,
        messages: &[Message],
        tools: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        let input = ProviderInput {
            profile: "test".into(),
            protocol: model::ApiProtocol::OpenAiCompletions,
            edit_tool: config::EditToolConfig::Edit,
            affinity_key: invocation.run_id.clone(),
            replay: None,
            request: ChatRequest {
                model: "test".into(),
                messages: messages.to_vec(),
                tools: tools.to_vec(),
                tool_choice: None,
                temperature: None,
                max_tokens: None,
                reasoning_effort: None,
                service_tier: None,
                output_schema: None,
                observation: None,
                responses_replay: None,
            },
        };
        self.inputs
            .lock()
            .unwrap()
            .entry(invocation.run_id.clone())
            .or_default()
            .push(input);
        let (root, child) = self.targets.lock().unwrap().unwrap();
        let first = invocation.run_id == root.to_string()
            && !self.requested_wait.swap(true, Ordering::SeqCst);
        if invocation.run_id == root.to_string() && !first {
            self.parent_resumed.notify_one();
        }
        if invocation.run_id == child.to_string() {
            self.child_entered.notify_one();
            self.child_release.acquire().await.unwrap().forget();
        }
        Ok(ChatResponse {
            message: Message {
                role: providers::Role::Assistant,
                content: if first {
                    ["wait-all", "wait-legacy"]
                        .into_iter()
                        .enumerate()
                        .map(|(i, id)| ContentBlock::ToolUse {
                            id: id.into(),
                            name: "wait".into(),
                            input: if i == 0 {
                                json!({"run_ids":[child.to_string()],"mode":"all"})
                            } else {
                                json!({"run_id":child.to_string()})
                            },
                        })
                        .collect()
                } else {
                    vec![ContentBlock::Text {
                        text: "done".into(),
                    }]
                },
            },
            usage: Default::default(),
            usage_reported: true,
            finish_reason: if first {
                FinishReason::ToolUse
            } else {
                FinishReason::Stop
            },
        })
    }
}

fn process(
    config: &storage::StorageConfig,
    storage: &storage::Storage,
    model: Arc<WaitModel>,
) -> AgentRuntime {
    let bus = Arc::new(EventBus::new(256));
    AgentRuntime::new(bus.clone(), Arc::new(tools::ToolExecutor::new(bus)), model)
        .with_run_store(RunStore::open(config, storage.handle()).unwrap())
}

fn graph_evidence(
    config: &storage::StorageConfig,
) -> (
    Vec<storage::recovery::RecoveryManifestRecord>,
    Vec<storage::recovery::RecoveryClaim>,
) {
    let db = storage::Database::open(config).unwrap();
    (
        db.recovery_manifests()
            .unwrap()
            .into_iter()
            .map(|load| load.record.unwrap())
            .collect(),
        db.recovery_claims().unwrap(),
    )
}

async fn assert_recovery_cache(human_after_c3: bool) {
    let dir = tempfile::tempdir().unwrap();
    let config = storage::StorageConfig {
        db_path: dir.path().join("recovery-cache.db"),
        ..Default::default()
    };
    let storage = storage::Storage::open(config.clone()).unwrap();
    let model = Arc::new(WaitModel {
        targets: Mutex::new(None),
        requested_wait: AtomicBool::new(false),
        child_release: Semaphore::new(0),
        child_entered: Notify::new(),
        parent_resumed: Notify::new(),
        inputs: Mutex::default(),
    });
    let runtime = process(&config, &storage, model.clone());
    let mut events = runtime.shared.bus.subscribe();
    let root = runtime.reserve_run_id();
    runtime.spawn_reserved(root, None, Role::Orchestrator, "work", RunConfig::default());
    let child = runtime
        .delegate_background_as_child(root, Role::Worker, "child", RunConfig::default())
        .unwrap();
    *model.targets.lock().unwrap() = Some((root, child));
    loop {
        let event = events.recv().await.unwrap();
        if let EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
            ref run_id,
            to: AgentRunPhase::Done | AgentRunPhase::Error | AgentRunPhase::Stopped,
            ..
        }) = event.kind
        {
            assert_ne!(*run_id, root.to_string(), "root ended before starting wait");
        }
        if matches!(event.kind, EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
            run_id, to: AgentRunPhase::Waiting, ..
        }) if run_id == root.to_string())
        {
            break;
        }
    }
    model.child_entered.notified().await;
    let mut changed = runtime.shared.quiesce.subscribe();
    runtime.request_quiesce();
    loop {
        assert!(
            !matches!(runtime.quiesce_state(), QuiesceState::Failed { .. }),
            "{:?}",
            runtime.quiesce_state()
        );
        if runtime.resume_intents().unwrap().iter().any(|i| {
            i.run_id == root
                && i.checkpoint.as_ref().is_some_and(|c| {
                    c.boundary == crate::checkpoint::CheckpointBoundary::ToolsWaiting
                })
        }) {
            break;
        }
        changed.changed().await.unwrap();
    }
    // Cancellation is deliberately after the durable parent ACK: whether the
    // child completes cannot determine the checkpoint boundary by scheduling.
    runtime.cancel(child).unwrap();
    let _ = runtime.wait(child).await;
    runtime.wait_quiesced().await.unwrap();
    let fence = runtime.fence_dispatch_restart().await.unwrap();
    runtime.drain_dispatch_tails(&fence).await.unwrap();
    let records = runtime
        .capture_recovery_records(
            &fence,
            &BTreeMap::from([(
                root,
                RecoveryRecipe {
                    settings_reference: crate::quiesce::CORE_RECOVERY_SETTINGS_REFERENCE.into(),
                    policy: ResumePolicy::CoreOwned,
                    goal_id: None,
                    team: None,
                    lane: None,
                    descriptor: None,
                },
            )]),
        )
        .unwrap();
    assert_eq!(
        records
            .iter()
            .find(|r| r.run_id == root)
            .unwrap()
            .waits
            .len(),
        2
    );
    runtime.save_recovery_graph("wire-cache", &records).unwrap();
    let prefix = runtime
        .shared
        .run_store
        .get()
        .unwrap()
        .restore_bundle(root)
        .unwrap()
        .unwrap();
    let before = model.inputs.lock().unwrap()[&root.to_string()][0].clone();
    drop(changed);
    drop(runtime);
    storage.close();
    // Reopen the writer, not merely another reader of the same open handle.
    let storage = storage::Storage::open(config.clone()).unwrap();
    let runtime = process(&config, &storage, model.clone());
    let leases = storage::shared_with_wait(
        &config.db_path,
        &dir.path().join("ownership"),
        std::time::Duration::ZERO,
    )
    .unwrap();
    let startup = leases
        .recovery_startup_complete(&config.db_path, &dir.path().join("ownership"))
        .unwrap();
    let plan = runtime.recovery_plan().unwrap();
    assert_eq!(plan.components.len(), 1, "{plan:?}");
    let mut paused = runtime
        .prepare_component(&plan.components[0], startup, &dir.path().join("ownership"))
        .unwrap();
    runtime
        .restore_relations_and_continuations(&mut paused)
        .unwrap();
    let owner = crate::ownership::OwnerHost::open(
        &dir.path().join("owner"),
        Default::default(),
        Arc::new(EventBus::new(64)),
    )
    .unwrap();
    let current = RunConfig {
        ownership: Some(owner.start("current").unwrap()),
        ..Default::default()
    };
    let authority = crate::CurrentRecoveryAuthority {
        members: BTreeMap::from([(root, current.clone())]),
    };
    if human_after_c3 {
        let execution = runtime.execution_snapshot(root).unwrap();
        runtime.on_execution(
            crate::runtime::HookPoint::RecoveryActivated,
            &execution,
            || {
                panic!("c3: FULL activation ACK before publication");
            },
        );
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                runtime.activate_component(paused, authority)
            }))
            .is_err()
        );
        assert!(runtime.list_agents().is_empty());
        assert_eq!(model.inputs.lock().unwrap()[&root.to_string()].len(), 1);
        drop(runtime);
        storage.close();
        let storage = storage::Storage::open(config.clone()).unwrap();
        let runtime = process(&config, &storage, model.clone());
        let plan = runtime.recovery_plan().unwrap();
        assert!(plan.components.is_empty());
        for reason in [
            RecoveryReason::SupersededManifest,
            RecoveryReason::NoCommittedManifest,
        ] {
            assert!(
                plan.report.skipped.iter().any(|s| s.reason == reason),
                "{plan:?}"
            );
        }
        let evidence = graph_evidence(&config);
        runtime
            .continue_goal(root, "human Continue".into(), current)
            .unwrap();
        model.parent_resumed.notified().await;
        assert_eq!(graph_evidence(&config), evidence);
        assert!(runtime.list_agents().iter().all(|a| a.run_id != child));
        runtime.cancel(root).unwrap();
        let _ = runtime.wait(root).await;
        drop(runtime);
        storage.close();
    } else {
        runtime.activate_component(paused, authority).unwrap();
        let evidence = graph_evidence(&config);
        model.parent_resumed.notified().await;
        assert_eq!(graph_evidence(&config), evidence);
        assert!(runtime.list_agents().iter().any(|a| a.run_id == child
            && matches!(
                a.phase,
                AgentRunPhase::Done | AgentRunPhase::Error | AgentRunPhase::Stopped
            )));
        runtime.cancel(root).unwrap();
        let _ = runtime.wait(root).await;
        drop(runtime);
        storage.close();
    }
    let inputs = model.inputs.lock().unwrap().clone();
    assert_eq!(
        inputs[&child.to_string()].len(),
        1,
        "child was not replayed"
    );
    assert_eq!(inputs[&root.to_string()].len(), 2);
    let after = &inputs[&root.to_string()][1];
    assert_eq!(after.profile, before.profile);
    assert_eq!(after.protocol, before.protocol);
    assert_eq!(after.affinity_key, before.affinity_key);
    let saved: Vec<Message> = serde_json::from_str(&prefix.record.messages_json).unwrap();
    assert_eq!(&after.request.messages[..saved.len()], saved);
    let outcomes: Vec<_> = after
        .request
        .messages
        .iter()
        .flat_map(|m| &m.content)
        .filter_map(|b| {
            if let ContentBlock::ToolResult { is_error, .. } = b {
                Some(*is_error)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(outcomes, [human_after_c3; 2]);
    let submissions: Vec<_> = after
        .request
        .messages
        .iter()
        .enumerate()
        .filter_map(|(i, m)| {
            m.content
                .iter()
                .any(|b| matches!(b, ContentBlock::Text {text} if text == "human Continue"))
                .then_some(i)
        })
        .collect();
    if human_after_c3 {
        assert_eq!(submissions.len(), 1);
        let last_result = after
            .request
            .messages
            .iter()
            .rposition(|m| {
                m.content
                    .iter()
                    .any(|b| matches!(b, ContentBlock::ToolResult { .. }))
            })
            .unwrap();
        assert!(last_result < submissions[0]);
    } else {
        assert!(submissions.is_empty());
    }
    crate::recovery_cache_test_support::assert_wait_cache(
        &before.request,
        &after.request,
        &["wait-all", "wait-legacy"],
    )
    .await;
}

#[tokio::test]
async fn tools_waiting_cache_survives_file_close_reopen_and_component_activation() {
    assert_recovery_cache(false).await;
}

#[tokio::test]
async fn tools_waiting_cache_survives_c3_file_reopen_and_explicit_continue_without_replay() {
    assert_recovery_cache(true).await;
}
