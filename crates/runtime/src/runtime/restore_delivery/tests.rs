use super::*;

#[derive(Default)]
struct CompletingModel(std::sync::Mutex<Vec<crate::AgentInvocationContext>>);

#[async_trait::async_trait]
impl AgentModel for CompletingModel {
    async fn complete(
        &self,
        invocation: &crate::AgentInvocationContext,
        _: Role,
        _: &[providers::Message],
        _: &[providers::ToolSpec],
    ) -> Result<providers::ChatResponse, RuntimeError> {
        self.0.lock().unwrap().push(invocation.clone());
        Ok(providers::ChatResponse {
            message: providers::Message {
                role: providers::Role::Assistant,
                content: vec![providers::ContentBlock::Text {
                    text: "done".into(),
                }],
            },
            finish_reason: providers::FinishReason::Stop,
            usage: providers::Usage::default(),
        })
    }

    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "test".into()
    }
}

#[tokio::test]
async fn live_delivery_restores_when_mailbox_closed_before_push() {
    test_support::isolated_state_dir();
    for phase in [AgentRunPhase::Done, AgentRunPhase::Running] {
        // Given: the completed snapshot/mailbox state at the live-to-terminal race seam.
        let dir = tempfile::tempdir().unwrap();
        let config = storage::StorageConfig {
            db_path: dir.path().join("race.sqlite3"),
            ..storage::StorageConfig::default()
        };
        let storage = storage::Storage::open(config.clone()).unwrap();
        let bus = Arc::new(EventBus::new(128));
        let executor = Arc::new(ToolExecutor::with_standard_tools(
            Arc::clone(&bus),
            Arc::new(sandbox::DirectSandbox::new_unchecked()),
        ));
        let runtime = AgentRuntime::new(bus, executor, Arc::new(CompletingModel::default()))
            .with_run_store(crate::RunStore::open(&config, storage.handle()).unwrap());
        let parent =
            runtime.delegate_background(Role::Orchestrator, "parent".into(), RunConfig::default());
        runtime.wait(parent).await.unwrap();
        let child = runtime
            .delegate_background_as_child(parent, Role::Worker, "child", RunConfig::default())
            .unwrap();
        runtime.wait(child).await.unwrap();
        lock_runs(&runtime.shared.runs)
            .get(&child)
            .unwrap()
            .phase_tx
            .send_replace(phase);
        // The deterministic seam is delivery after classification, not a scheduler-timed race.
        // When: the live delivery function reaches the now-closed mailbox.
        let result =
            runtime.prepare_delivery(parent, child, AgentMessageKind::Send, "resume".into(), None);
        // Then: exactly one restored trigger is accepted and processed.
        let (_, _, disposition) = result.unwrap();
        assert_eq!(disposition, DeliveryDisposition::Restored);
        assert_eq!(runtime.wait(child).await.unwrap(), AgentRunPhase::Done);
    }
}

#[tokio::test]
async fn message_driven_restore_ignores_quoted_huge_handles_and_preserves_history() {
    test_support::isolated_state_dir();
    let dir = tempfile::tempdir().unwrap();
    let config = storage::StorageConfig {
        db_path: dir.path().join("handles.sqlite3"),
        ..storage::StorageConfig::default()
    };
    let storage = storage::Storage::open(config.clone()).unwrap();
    let bus = Arc::new(EventBus::new(128));
    let model = Arc::new(CompletingModel::default());
    let runtime = AgentRuntime::new(bus.clone(), Arc::new(ToolExecutor::new(bus)), model.clone())
        .with_run_store(crate::RunStore::open(&config, storage.handle()).unwrap());
    let parent =
        runtime.delegate_background(Role::Orchestrator, "parent".into(), RunConfig::default());
    runtime.wait(parent).await.unwrap();
    let child = runtime
        .delegate_background_as_child(parent, Role::Worker, "child", RunConfig::default())
        .unwrap();
    runtime.wait(child).await.unwrap();
    let store = runtime.shared.run_store.get().unwrap();
    let mut record = store.restore_record(child).unwrap().unwrap();
    let mut history: Vec<providers::Message> = serde_json::from_str(&record.messages_json).unwrap();
    history.insert(
        0,
        providers::Message {
            role: providers::Role::User,
            content: vec![providers::ContentBlock::Text {
                text: "saved job-42; review: job-4294967295 job-18446744073709551614 job-18446744073709551615 job-184467440737095516160".into(),
            }],
        },
    );
    record.messages_json = serde_json::to_string(&history).unwrap();
    storage.handle().upsert_run_context(&record).unwrap();
    let (_, _, disposition) = runtime
        .prepare_delivery(parent, child, AgentMessageKind::Send, "resume".into(), None)
        .unwrap();
    assert_eq!(disposition, DeliveryDisposition::Restored);
    assert_eq!(runtime.wait(child).await.unwrap(), AgentRunPhase::Done);
    let after = store.restore_record(child).unwrap().unwrap();
    let restored: Vec<providers::Message> = serde_json::from_str(&after.messages_json).unwrap();
    assert_eq!(&restored[..history.len()], history.as_slice());
    assert_eq!(model.0.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn cold_restore_distinguishes_a_root_parent_from_a_durable_subagent_parent() {
    test_support::isolated_state_dir();
    for nested in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let config = storage::StorageConfig {
            db_path: directory.path().join("ancestry.db"),
            ..Default::default()
        };
        let storage = storage::Storage::open(config.clone()).unwrap();
        let make_runtime = |model: Arc<CompletingModel>| {
            let bus = Arc::new(EventBus::new(128));
            AgentRuntime::new(bus.clone(), Arc::new(ToolExecutor::new(bus)), model)
                .with_run_store(crate::RunStore::open(&config, storage.handle()).unwrap())
        };
        let original = make_runtime(Arc::new(CompletingModel::default()));
        let root =
            original.delegate_background(Role::Orchestrator, "root".into(), RunConfig::default());
        original.wait(root).await.unwrap();
        let child = original
            .delegate_background_as_child(root, Role::Worker, "child", RunConfig::default())
            .unwrap();
        original.wait(child).await.unwrap();
        let recipient = if nested {
            let grandchild = original
                .delegate_background_as_child(child, Role::Worker, "nested", RunConfig::default())
                .unwrap();
            original.wait(grandchild).await.unwrap();
            grandchild
        } else {
            child
        };
        drop(original);
        let model = Arc::new(CompletingModel::default());
        let runtime = make_runtime(model.clone());
        // A live descendant may contact its durable parent without restoring the ancestors first.
        let sender = runtime.reserve_run_id();
        runtime.spawn_reserved(
            sender,
            Some(recipient),
            Role::Worker,
            "sender",
            RunConfig::default(),
        );
        runtime.wait(sender).await.unwrap();
        runtime
            .send_agent_message(sender, recipient, AgentMessageKind::Send, "resume", None)
            .unwrap();
        runtime.wait(recipient).await.unwrap();
        let observed = model.0.lock().unwrap();
        let turn = observed.last().unwrap().turn.as_ref().unwrap();
        assert!(turn.is_subagent);
        assert_eq!(turn.parent_subagent_id, nested.then(|| child.to_string()));
    }
}

#[tokio::test]
async fn restored_registration_has_a_fresh_completion_receipt() {
    test_support::isolated_state_dir();
    let dir = tempfile::tempdir().unwrap();
    let config = storage::StorageConfig {
        db_path: dir.path().join("generation.sqlite3"),
        ..Default::default()
    };
    let storage = storage::Storage::open(config.clone()).unwrap();
    let bus = Arc::new(EventBus::new(128));
    let runtime = AgentRuntime::new(
        bus.clone(),
        Arc::new(ToolExecutor::new(bus)),
        Arc::new(CompletingModel::default()),
    )
    .with_run_store(crate::RunStore::open(&config, storage.handle()).unwrap());
    let parent =
        runtime.delegate_background(Role::Orchestrator, "parent".into(), RunConfig::default());
    assert_eq!(runtime.wait(parent).await.unwrap(), AgentRunPhase::Done);
    let child = runtime
        .delegate_background_as_child(parent, Role::Worker, "child", RunConfig::default())
        .unwrap();
    assert_eq!(runtime.wait(child).await.unwrap(), AgentRunPhase::Done);
    let old = runtime
        .run_output(parent, child)
        .unwrap()
        .completion_notice
        .unwrap();
    runtime.observe_completion(&old);
    runtime
        .send_agent_message(parent, child, AgentMessageKind::Send, "continue", None)
        .unwrap();
    assert_eq!(runtime.wait(child).await.unwrap(), AgentRunPhase::Done);
    let new = runtime
        .run_output(parent, child)
        .unwrap()
        .completion_notice
        .unwrap();
    assert_ne!(old.generation, new.generation);
    let report = AgentMessage {
        message_id: "new-report".into(),
        sender_run_id: child.to_string(),
        recipient_run_id: parent.to_string(),
        kind: AgentMessageKind::Send,
        content: "done".into(),
        reply_to: None,
    };
    runtime.hold_queued_message(
        parent,
        crate::mailbox::QueuedMessage::completion(report, new.clone()),
    );
    runtime.observe_completion(&old);
    assert_eq!(
        runtime.shared.held_messages.lock().unwrap()[&parent].len(),
        1
    );
    runtime.observe_completion(&new);
    assert!(runtime.shared.held_messages.lock().unwrap()[&parent].is_empty());
}
