use super::*;
use providers::{ContentBlock, Message};
use std::sync::atomic::AtomicUsize;

struct NoReplayTool {
    name: &'static str,
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl tools::Tool for NoReplayTool {
    fn name(&self) -> &str {
        self.name
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }

    fn permissions(&self) -> tools::Permissions {
        tools::Permissions::process()
    }

    async fn execute(&self, _: serde_json::Value) -> Result<tools::ToolResult, tools::ToolError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        Err(tools::ToolError::Io {
            detail: "old tool/job must not be replayed".into(),
        })
    }
}

fn outcome_notices(messages: &[Message], call_id: &str) -> usize {
    messages
        .iter()
        .flat_map(|message| &message.content)
        .filter(|block| {
            matches!(block, ContentBlock::Text { text }
            if text.contains("ToolExecutionOutcomeUnknown") && text.contains(call_id))
        })
        .count()
}

#[derive(Default)]
struct RetryModel {
    reject: AtomicBool,
    panic_admission: AtomicBool,
    fail_completion: AtomicBool,
    hold: AtomicBool,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
    requests: Mutex<Vec<Vec<Message>>>,
}

#[async_trait::async_trait]
impl AgentModel for RetryModel {
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "retry-test".into()
    }

    fn requires_admission(&self) -> bool {
        true
    }

    async fn admit(&self, _: &crate::AgentInvocationContext, _: Role) -> Result<(), RuntimeError> {
        if self.hold.load(Ordering::Relaxed) {
            self.entered.notify_one();
            self.release.notified().await;
        }
        assert!(
            !self.panic_admission.load(Ordering::Relaxed),
            "admission panic"
        );
        if self.reject.load(Ordering::Relaxed) {
            return Err(RuntimeError::Model {
                reason: "rejected".into(),
            });
        }
        Ok(())
    }

    async fn complete(
        &self,
        context: &crate::AgentInvocationContext,
        role: Role,
        messages: &[Message],
        tools: &[providers::ToolSpec],
    ) -> Result<providers::ChatResponse, RuntimeError> {
        self.requests.lock().unwrap().push(messages.to_vec());
        if self.fail_completion.load(Ordering::Relaxed) {
            return Err(RuntimeError::Model {
                reason: "completion failed".into(),
            });
        }
        CompletingModel
            .complete(context, role, messages, tools)
            .await
    }
}

fn saved(fixture: &Fixture, run: RunId) -> storage::RunContextRecord {
    fixture
        .runtime
        .shared
        .run_store
        .get()
        .unwrap()
        .restore_record(run)
        .unwrap()
        .unwrap()
}

fn descriptor(record: &storage::RunContextRecord) -> RunRestoreDescriptor {
    serde_json::from_str(&record.config_json).unwrap()
}

async fn held(model: &RetryModel) {
    model.entered.notified().await;
}

async fn waiting(runtime: &AgentRuntime, run: RunId) {
    let mut phase = runtime.entry(run).unwrap().phase_rx.clone();
    while *phase.borrow_and_update() != AgentRunPhase::Waiting {
        phase.changed().await.unwrap();
    }
}

#[tokio::test]
async fn rejected_followup_then_resume_preserves_unknown_outcomes() {
    rejected_followups_with_unknown_outcomes(1).await;
}

#[tokio::test]
async fn rejected_followups_preserve_history_images_evidence_and_reference_across_restart() {
    rejected_followups_with_unknown_outcomes(2).await;
}

async fn rejected_followups_with_unknown_outcomes(failures: usize) {
    let model = Arc::new(RetryModel::default());
    let fixture = Fixture::with_model(model.clone());
    let calls = Arc::new(AtomicUsize::new(0));
    let mut executor = ToolExecutor::new(fixture.runtime.shared.bus.clone());
    for name in ["shell", "write", "read"] {
        executor
            .register(Arc::new(NoReplayTool {
                name,
                calls: calls.clone(),
            }))
            .unwrap();
    }
    let executor = Arc::new(executor);
    *fixture.runtime.shared.executor.lock().unwrap() = executor.clone();
    let run = fixture
        .runtime
        .delegate_chat(
            "retry",
            Role::Worker,
            "original".into(),
            RunConfig::default(),
        )
        .unwrap();
    fixture.runtime.wait(run).await.unwrap();
    let mut before = saved(&fixture, run);
    let mut config = descriptor(&before);
    let reference_id = config.pending_reference_id.clone().unwrap();
    config.interrupted_tool_calls = [
        ("unobserved-shell-jobs", "shell", false),
        ("cancelled-write", "write", true),
        ("incomplete-read", "read", false),
    ]
    .into_iter()
    .map(
        |(call_id, tool_name, result_observed)| crate::restore::InterruptedToolCall {
            call_id: call_id.into(),
            tool_name: tool_name.into(),
            result_observed,
            may_have_side_effects: tool_name != "read",
        },
    )
    .collect();
    before.config_json = serde_json::to_string(&config).unwrap();
    let old: Vec<Message> = serde_json::from_str(&before.messages_json).unwrap();
    let checkpoints = vec![crate::CompactionCheckpoint {
        id: "saved-summary".into(),
        summary: Message {
            role: providers::Role::User,
            content: vec![ContentBlock::Text {
                text: "saved summary".into(),
            }],
        },
        range: (0, 1),
        protects_history: false,
    }];
    before.checkpoints_json = serde_json::to_string(&checkpoints).unwrap();
    fixture
        .storage
        .handle()
        .upsert_run_context(&before)
        .unwrap();
    let before = saved(&fixture, run);
    let recovered = RestoredState::for_conversation(&before).unwrap().messages;
    for call in &config.interrupted_tool_calls {
        assert_eq!(outcome_notices(&recovered, &call.call_id), 1);
    }
    assert!(recovered.starts_with(&old));
    model.reject.store(true, Ordering::Relaxed);
    for (index, text) in ["first followup", "second followup"]
        .into_iter()
        .take(failures)
        .enumerate()
    {
        fixture
            .runtime
            .continue_goal(
                run,
                text.into(),
                RunConfig {
                    images: vec![crate::DelegateImage {
                        media_type: "image/png".into(),
                        data: "aW1hZ2U=".into(),
                    }],
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(fixture.runtime.wait_admission(run).await.is_err());
        let retry = saved(&fixture, run);
        assert!(retry.restorable);
        let history: Vec<Message> = serde_json::from_str(&retry.messages_json).unwrap();
        for call in &config.interrupted_tool_calls {
            assert_eq!(
                outcome_notices(&history, &call.call_id),
                1,
                "{}",
                call.call_id
            );
        }
        assert_eq!(history.len(), recovered.len() + index + 1);
        assert!(history.starts_with(&recovered));
        assert_eq!(
            descriptor(&retry).interrupted_tool_calls,
            config.interrupted_tool_calls
        );
        assert!(matches!(
            RestoredState::from_record(&retry),
            Err(RuntimeError::RunRestoreFailed {
                reason: RunRestoreFailure::UnsupportedConfig(_),
                ..
            })
        ));
        assert!(
            fixture
                .runtime
                .restore_diagnostics(run)
                .unwrap()
                .unwrap()
                .history_available_with_current_authority
        );
        assert_eq!(model.requests.lock().unwrap().len(), 1);
        assert_eq!(calls.load(Ordering::Relaxed), 0);
    }
    let after = saved(&fixture, run);
    let history: Vec<Message> = serde_json::from_str(&after.messages_json).unwrap();
    assert_eq!(&history[..old.len()], old);
    assert_eq!(history.len(), recovered.len() + failures);
    for (message, text) in history[recovered.len()..]
        .iter()
        .zip(["first followup", "second followup"])
    {
        assert_eq!(
            message.content,
            vec![
                ContentBlock::Text { text: text.into() },
                ContentBlock::Image {
                    media_type: "image/png".into(),
                    data: "aW1hZ2U=".into()
                },
            ]
        );
    }
    assert_eq!(after.checkpoints_json, before.checkpoints_json);
    assert!(descriptor(&after).pending_reference_id.is_none());
    let before_turn = descriptor(&before).turn.unwrap();
    let after_turn = descriptor(&after).turn.unwrap();
    assert_eq!(after_turn.origin.as_deref(), Some("human"));
    assert_eq!(
        after_turn.prompt_index,
        before_turn
            .prompt_index
            .map(|index| index + failures as u64)
    );
    assert_eq!(
        after_turn.turn_index,
        before_turn.turn_index.map(|index| index + failures as u64)
    );
    let store = fixture.runtime.shared.run_store.get().unwrap();
    let rows = store
        .review_evidence_for_lineage(&[run.to_string()])
        .unwrap();
    assert_eq!(rows.len(), 1 + failures);
    let reference: serde_json::Value = serde_json::from_str(
        rows.iter()
            .find(|row| row.text == "first followup")
            .unwrap()
            .reference_context
            .as_deref()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(reference["text"], "done");
    assert_eq!(reference["source_run_id"], run.to_string());
    assert!(
        rows.iter()
            .filter(|row| row.text == "second followup")
            .all(|row| row.reference_context.is_none())
    );
    assert!(
        fixture
            .storage
            .handle()
            .assistant_reference(&reference_id)
            .unwrap()
            .unwrap()
            .consumed
    );

    let storage_config = storage::StorageConfig {
        db_path: fixture._dir.path().join("goal.sqlite3"),
        ..Default::default()
    };
    let bus = Arc::new(EventBus::new(128));
    let runtime = AgentRuntime::new(bus, executor, model.clone())
        .with_run_store(crate::RunStore::open(&storage_config, fixture.storage.handle()).unwrap());
    model.reject.store(false, Ordering::Relaxed);
    runtime.resume_chat(run, RunConfig::default()).unwrap();
    runtime.wait_admission(run).await.unwrap();
    waiting(&runtime, run).await;
    assert_eq!(model.requests.lock().unwrap().len(), 2);
    for call in &config.interrupted_tool_calls {
        assert_eq!(
            outcome_notices(&model.requests.lock().unwrap()[1], &call.call_id),
            1,
            "{}",
            call.call_id
        );
    }
    assert_eq!(
        model.requests.lock().unwrap()[1],
        checkpoints[0].project(&history)
    );
    assert_eq!(
        saved(&fixture, run).checkpoints_json,
        before.checkpoints_json
    );
    assert_eq!(
        store
            .review_evidence_for_lineage(&[run.to_string()])
            .unwrap(),
        rows
    );
    assert_eq!(calls.load(Ordering::Relaxed), 0);
    // A later execution may leave different jobs unobserved under the same
    // synthetic ID. A fresh snapshot must not mistake those for the old jobs.
    let mut fresh = saved(&fixture, run);
    let mut fresh_config = descriptor(&fresh);
    fresh_config.interrupted_tool_calls = config.interrupted_tool_calls.clone();
    fresh.config_json = serde_json::to_string(&fresh_config).unwrap();
    let fresh_history: Vec<Message> = serde_json::from_str(&fresh.messages_json).unwrap();
    let newly_recovered = RestoredState::for_conversation(&fresh).unwrap().messages;
    assert!(newly_recovered.starts_with(&fresh_history));
    assert_eq!(
        outcome_notices(&newly_recovered, "unobserved-shell-jobs"),
        2
    );
    assert_eq!(outcome_notices(&newly_recovered, "cancelled-write"), 1);
    assert_eq!(outcome_notices(&newly_recovered, "incomplete-read"), 1);
    assert_eq!(
        runtime.shared.review_runs.lock().unwrap()[&run]
            .requests()
            .0
            .len(),
        1 + failures
    );
    runtime.cancel(run).unwrap();
    runtime.wait(run).await.unwrap();
}

#[tokio::test]
async fn cancelled_admission_releases_only_its_reservation_and_success_commits_consumption() {
    let model = Arc::new(RetryModel::default());
    let fixture = Fixture::with_model(model.clone());
    let run = fixture.terminal().await;
    model.hold.store(true, Ordering::Relaxed);
    fixture
        .runtime
        .resume_chat(run, RunConfig::default())
        .unwrap();
    held(&model).await;
    assert!(!saved(&fixture, run).restorable);
    assert!(matches!(
        fixture.runtime.resume_chat(run, RunConfig::default()),
        Err(RuntimeError::RunRestoreFailed {
            reason: RunRestoreFailure::UnsupportedConfig(ref reason),
            ..
        }) if reason == "snapshot_consumed"
    ));
    // The durable fence must also reject a runtime with no in-memory admission.
    let storage_config = storage::StorageConfig {
        db_path: fixture._dir.path().join("goal.sqlite3"),
        ..Default::default()
    };
    let bus = Arc::new(EventBus::new(128));
    let other = AgentRuntime::new(bus.clone(), Arc::new(ToolExecutor::new(bus)), model.clone())
        .with_run_store(crate::RunStore::open(&storage_config, fixture.storage.handle()).unwrap());
    assert!(matches!(
        other.resume_chat(run, RunConfig::default()),
        Err(RuntimeError::RunRestoreFailed {
            reason: RunRestoreFailure::UnsupportedConfig(ref reason),
            ..
        }) if reason == "snapshot_consumed"
    ));
    fixture.runtime.cancel(run).unwrap();
    model.release.notify_one();
    assert!(matches!(
        fixture.runtime.wait_admission(run).await,
        Err(RuntimeError::RunTerminated { .. })
    ));
    assert!(saved(&fixture, run).restorable);

    fixture
        .runtime
        .resume_chat(run, RunConfig::default())
        .unwrap();
    held(&model).await;
    model.release.notify_one();
    fixture.runtime.wait_admission(run).await.unwrap();
    // Registration must either keep consumption or replace it with a fresh loop snapshot.
    let after = saved(&fixture, run);
    if descriptor(&after).admission_reservation_id.is_some() {
        assert!(!after.restorable);
    } else {
        assert_eq!(after.terminal_phase, "Checkpoint");
    }
    assert!(matches!(
        *fixture.runtime.entry(run).unwrap().phase_rx.borrow(),
        AgentRunPhase::Pending | AgentRunPhase::Running | AgentRunPhase::Waiting
    ));
    fixture.runtime.cancel(run).unwrap();
    fixture.runtime.wait(run).await.unwrap();
}

#[tokio::test]
async fn stop_during_wake_admission_keeps_snapshot_and_held_message_for_operator_resume() {
    let model = Arc::new(RetryModel::default());
    let fixture = Fixture::with_model(model.clone());
    let runtime = &fixture.runtime;
    let root = fixture.terminal().await;
    let old_entry = Arc::clone(&runtime.entry(root).unwrap().mailbox);
    let before = saved(&fixture, root);
    let report = AgentMessage {
        message_id: "held-during-admission".into(),
        sender_run_id: RunId::new(123).to_string(),
        recipient_run_id: root.to_string(),
        kind: AgentMessageKind::Send,
        content: "held during admission".into(),
        reply_to: None,
    };
    {
        let _runs = lock_runs(&runtime.shared.runs);
        runtime.hold_message(root, report.clone());
    }
    model.hold.store(true, Ordering::Relaxed);
    runtime.wake_chat(root, RunConfig::default()).unwrap();
    held(&model).await;
    runtime.stop(root, StopScope::SelfOnly).unwrap();
    model.release.notify_one();
    assert!(matches!(
        runtime.wait_admission(root).await,
        Err(RuntimeError::RunTerminated { .. })
    ));
    assert_eq!(saved(&fixture, root).restorable, before.restorable);
    assert_eq!(saved(&fixture, root).messages_json, before.messages_json);
    assert_eq!(saved(&fixture, root).config_json, before.config_json);
    assert!(Arc::ptr_eq(
        &old_entry,
        &runtime.entry(root).unwrap().mailbox
    ));
    assert_eq!(
        runtime.shared.held_messages.lock().unwrap()[&root],
        vec![report]
    );
    assert!(matches!(
        runtime.wake_chat(root, RunConfig::default()),
        Err(RuntimeError::RunRestoreFailed {
            reason: RunRestoreFailure::StoppedByOperator,
            ..
        })
    ));
    model.hold.store(false, Ordering::Relaxed);
    runtime.resume_chat(root, RunConfig::default()).unwrap();
    runtime.wait_admission(root).await.unwrap();
    waiting(runtime, root).await;
    assert!(!runtime.operator_stopped(root));
    assert!(
        !runtime
            .shared
            .held_messages
            .lock()
            .unwrap()
            .contains_key(&root)
    );
    {
        let requests = model.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(
            serde_json::to_string(&requests[1])
                .unwrap()
                .matches("held during admission")
                .count(),
            1
        );
    }
    runtime.cancel(root).unwrap();
    runtime.wait(root).await.unwrap();
}

#[tokio::test]
async fn stopped_wake_preserves_snapshot_and_auto_wake_budget_without_reserving() {
    // Given: a stopped root with a report held for it and part of its wake budget used.
    let model = Arc::new(RetryModel::default());
    let fixture = Fixture::with_model(model.clone());
    let run = fixture.terminal().await;
    fixture.runtime.hold_message(
        run,
        AgentMessage {
            message_id: "held-report".into(),
            sender_run_id: RunId::new(99).to_string(),
            recipient_run_id: run.to_string(),
            kind: AgentMessageKind::Send,
            content: "held child report".into(),
            reply_to: None,
        },
    );
    fixture
        .runtime
        .shared
        .operator_stops
        .lock()
        .unwrap()
        .insert(run);
    fixture
        .runtime
        .shared
        .auto_wakes
        .lock()
        .unwrap()
        .insert(run, 3);
    let before = saved(&fixture, run);
    // A runtime-requested wake is refused before it can reserve the snapshot.
    assert!(matches!(
        fixture.runtime.wake_chat(run, RunConfig::default()),
        Err(RuntimeError::RunRestoreFailed {
            reason: RunRestoreFailure::StoppedByOperator,
            ..
        })
    ));
    let after = saved(&fixture, run);
    assert_eq!(after, before);
    assert!(after.restorable);
    assert!(descriptor(&after).restorable);
    assert!(descriptor(&after).non_restorable_reason.is_none());
    assert!(descriptor(&after).admission_reservation_id.is_none());
    assert!(fixture.runtime.operator_stopped(run));
    assert_eq!(
        fixture.runtime.shared.auto_wakes.lock().unwrap().get(&run),
        Some(&3)
    );
    assert_eq!(
        fixture
            .runtime
            .shared
            .held_messages
            .lock()
            .unwrap()
            .get(&run)
            .map(Vec::len),
        Some(1)
    );

    // When: the operator explicitly resumes.
    fixture
        .runtime
        .resume_chat(run, RunConfig::default())
        .unwrap();
    fixture.runtime.wait_admission(run).await.unwrap();
    // Then: the explicit resume lifts the stop and renews the budget, and
    // registration hands the held report to the restored run.
    assert!(!fixture.runtime.operator_stopped(run));
    assert!(
        !fixture
            .runtime
            .shared
            .auto_wakes
            .lock()
            .unwrap()
            .contains_key(&run)
    );
    assert!(
        !fixture
            .runtime
            .shared
            .held_messages
            .lock()
            .unwrap()
            .contains_key(&run)
    );
    tokio::time::timeout(Duration::from_secs(10), async {
        while !model.requests.lock().unwrap().iter().any(|request| {
            serde_json::to_string(request)
                .unwrap()
                .contains("held child report")
        }) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the restored run receives the held report");
    fixture.runtime.cancel(run).unwrap();
    fixture.runtime.wait(run).await.unwrap();
}

#[tokio::test]
async fn failed_attempt_does_not_replace_a_newer_snapshot_or_its_reservation() {
    for new_reservation in [false, true] {
        let model = Arc::new(RetryModel::default());
        let fixture = Fixture::with_model(model.clone());
        let run = fixture.terminal().await;
        model.hold.store(true, Ordering::Relaxed);
        model.reject.store(true, Ordering::Relaxed);
        fixture
            .runtime
            .resume_chat(run, RunConfig::default())
            .unwrap();
        held(&model).await;
        let mut newer = saved(&fixture, run);
        let mut config = descriptor(&newer);
        config.admission_reservation_id = new_reservation.then(|| "another-attempt".into());
        if !new_reservation {
            config.non_restorable_reason = Some("persist_failed".into());
        }
        newer.config_json = serde_json::to_string(&config).unwrap();
        newer.updated_at_ns += 1;
        fixture.storage.handle().upsert_run_context(&newer).unwrap();
        model.release.notify_one();
        assert!(fixture.runtime.wait_admission(run).await.is_err());
        assert_eq!(saved(&fixture, run), newer);
    }
}

#[tokio::test]
async fn admission_panic_releases_snapshot_but_post_registration_model_error_uses_finalization() {
    let model = Arc::new(RetryModel::default());
    let fixture = Fixture::with_model(model.clone());
    let run = fixture.terminal().await;
    model.panic_admission.store(true, Ordering::Relaxed);
    fixture
        .runtime
        .resume_chat(run, RunConfig::default())
        .unwrap();
    assert!(fixture.runtime.wait_admission(run).await.is_err());
    assert!(saved(&fixture, run).restorable);
    model.panic_admission.store(false, Ordering::Relaxed);
    model.fail_completion.store(true, Ordering::Relaxed);
    fixture
        .runtime
        .continue_goal(run, "fail after admission".into(), RunConfig::default())
        .unwrap();
    fixture.runtime.wait_admission(run).await.unwrap();
    assert_eq!(
        fixture.runtime.wait(run).await.unwrap(),
        AgentRunPhase::Error
    );
    let after = saved(&fixture, run);
    assert!(after.restorable);
    assert_eq!(after.terminal_phase, "Error");
    assert!(descriptor(&after).admission_reservation_id.is_none());
    model.fail_completion.store(false, Ordering::Relaxed);
    fixture
        .runtime
        .resume_chat(run, RunConfig::default())
        .unwrap();
    fixture.runtime.wait_admission(run).await.unwrap();
    waiting(&fixture.runtime, run).await;
    fixture.runtime.cancel(run).unwrap();
    fixture.runtime.wait(run).await.unwrap();
}

#[tokio::test]
async fn committed_reservation_drop_does_not_restore_consumed_snapshot() {
    let fixture = Fixture::new();
    let run = fixture.terminal().await;
    let store = fixture.runtime.shared.run_store.get().unwrap();
    let mut record = saved(&fixture, run);
    let mut config = descriptor(&record);
    let reservation = SnapshotReservation::new(store, run, &record, &config).unwrap();
    config.admission_reservation_id = Some(reservation.id().into());
    config.restorable = false;
    config.non_restorable_reason = Some("snapshot_consumed".into());
    record.restorable = false;
    record.config_json = serde_json::to_string(&config).unwrap();
    fixture
        .storage
        .handle()
        .upsert_run_context(&record)
        .unwrap();
    let mut reservation = reservation.arm(&config);
    reservation.commit();
    drop(reservation);
    assert_eq!(saved(&fixture, run), record);
}

#[tokio::test]
async fn unavailable_storage_keeps_failed_admission_consumption_fail_closed() {
    let model = Arc::new(RetryModel::default());
    let fixture = Fixture::with_model(model.clone());
    let run = fixture.terminal().await;
    model.hold.store(true, Ordering::Relaxed);
    model.reject.store(true, Ordering::Relaxed);
    fixture
        .runtime
        .resume_chat(run, RunConfig::default())
        .unwrap();
    held(&model).await;
    let store = fixture.runtime.shared.run_store.get().unwrap();
    fixture.storage.close();
    model.release.notify_one();
    assert!(fixture.runtime.wait_admission(run).await.is_err());
    let after = store.restore_record(run).unwrap().unwrap();
    assert!(!after.restorable);
    assert_eq!(
        descriptor(&after).non_restorable_reason.as_deref(),
        Some("snapshot_consumed")
    );
}

#[tokio::test]
async fn failed_renewable_restore_retains_its_original_authority_gate() {
    let model = Arc::new(RetryModel::default());
    let fixture = Fixture::with_model(model.clone());
    let run = fixture.terminal().await;
    let mut before = saved(&fixture, run);
    let mut config = descriptor(&before);
    config.restorable = false;
    config.non_restorable_reason =
        Some(crate::restore::ROOT_CONTEXT_RENEWAL_REQUIRED_REASON.into());
    before.restorable = false;
    before.config_json = serde_json::to_string(&config).unwrap();
    fixture
        .storage
        .handle()
        .upsert_run_context(&before)
        .unwrap();
    model.reject.store(true, Ordering::Relaxed);
    fixture
        .runtime
        .resume_chat(run, RunConfig::default())
        .unwrap();
    assert!(fixture.runtime.wait_admission(run).await.is_err());
    assert_eq!(saved(&fixture, run), before);
    // Retry still goes through renewed root authority, rather than becoming disk-restorable.
    fixture
        .runtime
        .resume_chat(run, RunConfig::default())
        .unwrap();
    assert!(fixture.runtime.wait_admission(run).await.is_err());
}
