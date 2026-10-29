use super::*;
use providers::{ChatResponse, Message, ToolSpec};
use tokio::sync::Notify;

#[derive(Default)]
struct Model {
    requests: Mutex<Vec<Vec<Message>>>,
    summarizing: Notify,
    release: Notify,
}

#[async_trait::async_trait]
impl AgentModel for Model {
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "test".into()
    }

    async fn complete(
        &self,
        invocation: &crate::AgentInvocationContext,
        role: Role,
        messages: &[Message],
        tools: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        if invocation.purpose == event_bus::RequestPurpose::Compaction {
            self.summarizing.notify_one();
            self.release.notified().await;
        } else {
            self.requests.lock().unwrap().push(messages.to_vec());
        }
        CompletingModel
            .complete(invocation, role, messages, tools)
            .await
    }
}

async fn wait_for_waiting(runtime: &AgentRuntime, run: RunId) {
    runtime.wait_admission(run).await.unwrap();
    let mut phase = runtime.entry(run).unwrap().phase_rx.clone();
    loop {
        if *phase.borrow_and_update() == AgentRunPhase::Waiting {
            return;
        }
        phase.changed().await.unwrap();
    }
}

async fn seed(fixture: &Fixture) -> RunId {
    let run = fixture.terminal().await;
    fixture
        .runtime
        .continue_goal(run, "second turn".into(), RunConfig::default())
        .unwrap();
    wait_for_waiting(&fixture.runtime, run).await;
    fixture.runtime.stop(run, StopScope::SelfOnly).unwrap();
    assert_eq!(
        fixture.runtime.wait(run).await.unwrap(),
        AgentRunPhase::Stopped
    );
    run
}

fn configured(model: Arc<Model>, max_bytes: u64) -> Fixture {
    let mut fixture = Fixture::with_model(model);
    fixture.runtime = fixture.runtime.with_compaction(config::CompactionConfig {
        context_window_tokens: 1_000_000,
        keep_recent_tokens: 1,
        max_summary_bytes: max_bytes,
        ..Default::default()
    });
    fixture
}

fn snapshot(fixture: &Fixture, run: RunId) -> storage::thread_index::SnapshotBundle {
    fixture
        .runtime
        .shared
        .run_store
        .get()
        .unwrap()
        .restore_bundle(run)
        .unwrap()
        .unwrap()
}

fn compact(
    runtime: &AgentRuntime,
    run: RunId,
) -> tokio::task::JoinHandle<Result<(), RuntimeError>> {
    let runtime = runtime.clone();
    tokio::spawn(async move { runtime.compact_saved_chat(run, RunConfig::default()).await })
}

#[tokio::test]
async fn saved_compaction_serializes_mutations_and_resumes_without_replaying_a_completed_turn() {
    let model = Arc::new(Model::default());
    let fixture = configured(model.clone(), 16_384);
    let run = seed(&fixture).await;
    let before = snapshot(&fixture, run);
    let job = compact(&fixture.runtime, run);
    model.summarizing.notified().await;
    assert_eq!(snapshot(&fixture, run), before);
    assert_eq!(
        fixture.runtime.inspect_agent(run).unwrap().phase,
        AgentRunPhase::Stopped
    );
    for result in [
        fixture
            .runtime
            .resume_chat(run, RunConfig::default())
            .map(|_| ()),
        fixture
            .runtime
            .continue_goal(run, "must not be accepted".into(), RunConfig::default())
            .map(|_| ()),
        fixture.runtime.stop(run, StopScope::SelfOnly),
        fixture
            .runtime
            .compact_saved_chat(run, RunConfig::default())
            .await,
    ] {
        assert!(matches!(
            result,
            Err(RuntimeError::CompactionInFlight { .. })
        ));
    }
    model.release.notify_one();
    job.await.unwrap().unwrap();
    let saved = snapshot(&fixture, run);
    assert!(saved.snapshot_revision > before.snapshot_revision);
    assert_eq!(saved.record.messages_json, before.record.messages_json);
    assert_eq!(saved.record.terminal_phase, "Stopped");
    assert_ne!(
        saved.record.checkpoints_json,
        before.record.checkpoints_json
    );
    assert_eq!(
        saved.record.host_inputs_json,
        before.record.host_inputs_json
    );
    assert_eq!(
        saved.provenance.as_ref().unwrap().status,
        before.provenance.as_ref().unwrap().status
    );
    assert_eq!(model.requests.lock().unwrap().len(), 2);
    assert_eq!(fixture.runtime.list_agents().len(), 1);
    fixture
        .runtime
        .resume_chat(
            run,
            RunConfig {
                interactive: true,
                keep_alive: false,
                ..Default::default()
            },
        )
        .unwrap();
    wait_for_waiting(&fixture.runtime, run).await;
    assert_eq!(model.requests.lock().unwrap().len(), 2);
    let mut phase = fixture.runtime.entry(run).unwrap().phase_rx.clone();
    phase.borrow_and_update();
    fixture
        .runtime
        .send_message(run, "continue now".into())
        .unwrap();
    loop {
        phase.changed().await.unwrap();
        if *phase.borrow_and_update() == AgentRunPhase::Waiting {
            break;
        }
    }
    let requests = model.requests.lock().unwrap();
    let request = requests.last().unwrap();
    assert!(request.iter().flat_map(|m| &m.content).any(|b| matches!(b,
        providers::ContentBlock::Text { text } if text.contains("[COMPACTION CHECKPOINT "))));
}

#[tokio::test]
async fn cancelled_or_failed_saved_compaction_is_atomic_and_releases_the_reservation() {
    for cancel in [false, true] {
        let model = Arc::new(Model::default());
        let fixture = configured(model.clone(), 0);
        let run = seed(&fixture).await;
        let before = snapshot(&fixture, run);
        let job = compact(&fixture.runtime, run);
        model.summarizing.notified().await;
        if cancel {
            job.abort();
            assert!(job.await.unwrap_err().is_cancelled());
        } else {
            model.release.notify_one();
            assert!(matches!(
                job.await.unwrap(),
                Err(RuntimeError::SavedCompactionFailed { .. })
            ));
        }
        assert_eq!(snapshot(&fixture, run), before);
        assert_eq!(
            fixture.runtime.inspect_agent(run).unwrap().phase,
            AgentRunPhase::Stopped
        );
        fixture
            .runtime
            .resume_chat(run, RunConfig::default())
            .unwrap();
        wait_for_waiting(&fixture.runtime, run).await;
    }
}

#[tokio::test]
async fn snapshot_revision_conflict_does_not_overwrite_an_external_update() {
    let model = Arc::new(Model::default());
    let fixture = configured(model.clone(), 16_384);
    let run = seed(&fixture).await;
    let job = compact(&fixture.runtime, run);
    model.summarizing.notified().await;
    let mut updated = snapshot(&fixture, run).record;
    updated.terminal_phase = "Error".into();
    fixture
        .storage
        .handle()
        .upsert_run_context(&updated)
        .unwrap();
    model.release.notify_one();
    assert!(matches!(
        job.await.unwrap(),
        Err(RuntimeError::SavedCompactionFailed { .. })
    ));
    assert_eq!(snapshot(&fixture, run).record, updated);
    fixture.runtime.ensure_no_saved_compaction(run).unwrap();
}

#[tokio::test]
async fn ownership_lost_during_summary_cannot_commit_the_prepared_checkpoint() {
    let model = Arc::new(Model::default());
    let fixture = configured(model.clone(), 16_384);
    let run = seed(&fixture).await;
    let before = snapshot(&fixture, run);
    let host = crate::ownership::OwnerHost::open(
        fixture._dir.path(),
        Default::default(),
        Arc::new(EventBus::new(128)),
    )
    .unwrap();
    let permit = host.start("thread").unwrap();
    let runtime = fixture.runtime.clone();
    let job = tokio::spawn(async move {
        runtime
            .compact_saved_chat(
                run,
                RunConfig {
                    ownership: Some(permit),
                    ..Default::default()
                },
            )
            .await
    });
    model.summarizing.notified().await;
    assert!(!host.begin_quiesce().unwrap());
    assert!(!host.release_ready().unwrap());
    model.release.notify_one();
    assert!(job.await.unwrap().is_err());
    assert_eq!(snapshot(&fixture, run), before);
    fixture.runtime.ensure_no_saved_compaction(run).unwrap();
}

#[tokio::test]
async fn missing_disabled_live_and_unsafe_saved_contexts_fail_closed() {
    let model = Arc::new(Model::default());
    let fixture = configured(model, 16_384);
    let run = seed(&fixture).await;
    let mut record = snapshot(&fixture, run).record;
    let mut descriptor: RunRestoreDescriptor = serde_json::from_str(&record.config_json).unwrap();
    descriptor.restorable = false;
    descriptor.non_restorable_reason = Some("disabled".into());
    record.restorable = false;
    record.config_json = serde_json::to_string(&descriptor).unwrap();
    fixture
        .storage
        .handle()
        .upsert_run_context(&record)
        .unwrap();
    assert!(
        fixture
            .runtime
            .compact_saved_chat(run, RunConfig::default())
            .await
            .is_err()
    );
    assert_eq!(snapshot(&fixture, run).record, record);
    descriptor.restorable = true;
    descriptor.non_restorable_reason = None;
    descriptor
        .interrupted_tool_calls
        .push(crate::restore::InterruptedToolCall {
            call_id: "unresolved-write".into(),
            tool_name: "shell".into(),
            result_observed: false,
            may_have_side_effects: true,
        });
    record.restorable = true;
    record.config_json = serde_json::to_string(&descriptor).unwrap();
    fixture
        .storage
        .handle()
        .upsert_run_context(&record)
        .unwrap();
    assert!(
        fixture
            .runtime
            .compact_saved_chat(run, RunConfig::default())
            .await
            .is_err()
    );
    assert_eq!(snapshot(&fixture, run).record, record);
    assert!(
        fixture
            .runtime
            .compact_saved_chat(RunId::new(999999), RunConfig::default())
            .await
            .is_err()
    );
    let live = fixture.runtime.delegate_background(
        Role::Worker,
        "live".into(),
        RunConfig {
            interactive: true,
            keep_alive: true,
            ..Default::default()
        },
    );
    wait_for_waiting(&fixture.runtime, live).await;
    assert!(matches!(
        fixture
            .runtime
            .compact_saved_chat(live, RunConfig::default())
            .await,
        Err(RuntimeError::SavedCompactionFailed { .. })
    ));
}

#[tokio::test]
async fn disk_only_done_error_and_stopped_snapshots_compact_without_registering_a_run() {
    let model = Arc::new(Model::default());
    let fixture = configured(model.clone(), 16_384);
    let run = seed(&fixture).await;
    let original = snapshot(&fixture, run).record;
    for phase in ["Done", "Error", "Stopped"] {
        let mut record = original.clone();
        record.terminal_phase = phase.into();
        fixture
            .storage
            .handle()
            .upsert_run_context(&record)
            .unwrap();
        let bus = Arc::new(EventBus::new(128));
        let config = storage::StorageConfig {
            db_path: fixture._dir.path().join("goal.sqlite3"),
            ..Default::default()
        };
        let runtime =
            AgentRuntime::new(bus.clone(), Arc::new(ToolExecutor::new(bus)), model.clone())
                .with_run_store(crate::RunStore::open(&config, fixture.storage.handle()).unwrap())
                .with_compaction(config::CompactionConfig {
                    keep_recent_tokens: 1,
                    summarizer: config::SummarizerKind::Structural,
                    ..Default::default()
                });
        runtime
            .compact_saved_chat(run, RunConfig::default())
            .await
            .unwrap();
        assert!(runtime.list_agents().is_empty());
        let saved = snapshot(&fixture, run).record;
        assert_eq!(saved.messages_json, record.messages_json);
        assert_eq!(saved.terminal_phase, phase);
        assert_ne!(saved.checkpoints_json, record.checkpoints_json);
        assert_eq!(model.requests.lock().unwrap().len(), 2);
    }
}
