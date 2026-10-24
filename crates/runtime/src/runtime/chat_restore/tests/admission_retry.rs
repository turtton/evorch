use super::*;
use providers::{ContentBlock, Message};

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
async fn rejected_followups_preserve_history_images_evidence_and_reference_across_restart() {
    let model = Arc::new(RetryModel::default());
    let fixture = Fixture::with_model(model.clone());
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
    let before = saved(&fixture, run);
    let reference_id = descriptor(&before).pending_reference_id.unwrap();
    model.reject.store(true, Ordering::Relaxed);
    for text in ["first followup", "second followup"] {
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
        assert!(saved(&fixture, run).restorable);
    }
    let after = saved(&fixture, run);
    let history: Vec<Message> = serde_json::from_str(&after.messages_json).unwrap();
    let old: Vec<Message> = serde_json::from_str(&before.messages_json).unwrap();
    assert_eq!(&history[..old.len()], old);
    assert_eq!(history.len(), old.len() + 2);
    assert!(history[old.len()..].iter().all(|message| matches!(&message.content[1], ContentBlock::Image { data, .. } if data == "aW1hZ2U=")));
    assert_eq!(after.checkpoints_json, before.checkpoints_json);
    assert!(descriptor(&after).pending_reference_id.is_none());
    let before_turn = descriptor(&before).turn.unwrap();
    let after_turn = descriptor(&after).turn.unwrap();
    assert_eq!(after_turn.origin.as_deref(), Some("human"));
    assert_eq!(
        after_turn.prompt_index,
        before_turn.prompt_index.map(|index| index + 2)
    );
    assert_eq!(
        after_turn.turn_index,
        before_turn.turn_index.map(|index| index + 2)
    );
    let store = fixture.runtime.shared.run_store.get().unwrap();
    let rows = store
        .review_evidence_for_lineage(&[run.to_string()])
        .unwrap();
    assert_eq!(rows.len(), 3);
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
            .find(|row| row.text == "second followup")
            .unwrap()
            .reference_context
            .is_none()
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

    let config = storage::StorageConfig {
        db_path: fixture._dir.path().join("goal.sqlite3"),
        ..Default::default()
    };
    let bus = Arc::new(EventBus::new(128));
    let runtime = AgentRuntime::new(bus.clone(), Arc::new(ToolExecutor::new(bus)), model.clone())
        .with_run_store(crate::RunStore::open(&config, fixture.storage.handle()).unwrap());
    model.reject.store(false, Ordering::Relaxed);
    runtime.resume_chat(run, RunConfig::default()).unwrap();
    runtime.wait_admission(run).await.unwrap();
    waiting(&runtime, run).await;
    assert_eq!(model.requests.lock().unwrap().len(), 2);
    assert_eq!(model.requests.lock().unwrap()[1], history);
    assert_eq!(
        runtime.shared.review_runs.lock().unwrap()[&run]
            .requests()
            .0
            .len(),
        3
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
