use super::*;
use providers::{ChatResponse, ContentBlock, FinishReason, Message, ToolSpec};

struct RecordingModel {
    requests: Mutex<Vec<Vec<Message>>>,
    first_finish: FinishReason,
}

#[async_trait::async_trait]
impl AgentModel for RecordingModel {
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "test".into()
    }

    async fn complete(
        &self,
        context: &crate::AgentInvocationContext,
        role: Role,
        messages: &[Message],
        tools: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        let first = {
            let mut requests = self.requests.lock().unwrap();
            requests.push(messages.to_vec());
            requests.len() == 1
        };
        let mut response = CompletingModel
            .complete(context, role, messages, tools)
            .await?;
        if first {
            response.finish_reason = self.first_finish.clone();
        }
        Ok(response)
    }
}

async fn wait_for_waiting(runtime: &AgentRuntime, run: RunId) {
    let mut phase = runtime.entry(run).unwrap().phase_rx.clone();
    loop {
        let current = *phase.borrow_and_update();
        if current == AgentRunPhase::Waiting {
            return;
        }
        assert!(!matches!(
            current,
            AgentRunPhase::Done | AgentRunPhase::Error | AgentRunPhase::Stopped
        ));
        phase.changed().await.unwrap();
    }
}

fn record(fixture: &Fixture, run: RunId) -> storage::RunContextRecord {
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

#[tokio::test]
async fn completed_resume_preserves_history_goal_request_and_current_review_authority() {
    let model = Arc::new(RecordingModel {
        requests: Mutex::new(Vec::new()),
        first_finish: FinishReason::Stop,
    });
    let fixture = Fixture::with_model(model.clone());
    let runtime = &fixture.runtime;
    let run = runtime
        .delegate_chat(
            "thread",
            Role::Worker,
            "original request".into(),
            RunConfig {
                interactive: true,
                keep_alive: true,
                ..Default::default()
            },
        )
        .unwrap();
    wait_for_waiting(runtime, run).await;
    runtime.stop(run, StopScope::SelfOnly).unwrap();
    runtime.wait(run).await.unwrap();
    let before = record(&fixture, run);
    let original_request = runtime.trusted_thread_request(run);
    let review = runtime.shared.review_runs.lock().unwrap()[&run].clone();
    let original_user_requests =
        serde_json::to_value(&*review.user_requests.lock().unwrap()).unwrap();
    runtime.resume_chat(run, RunConfig::default()).unwrap();
    wait_for_waiting(runtime, run).await;
    assert_eq!(model.requests.lock().unwrap().len(), 1);
    assert_eq!(record(&fixture, run).messages_json, before.messages_json);
    assert_eq!(runtime.trusted_thread_request(run), original_request);
    let resumed_review = runtime.shared.review_runs.lock().unwrap()[&run].clone();
    assert_eq!(
        serde_json::to_value(&*resumed_review.user_requests.lock().unwrap()).unwrap(),
        original_user_requests
    );
    // A second resume on a waiting run is a no-op, including its inbox and settings.
    let before_config = runtime.entry(run).unwrap().config.clone();
    runtime.resume_chat(run, RunConfig::default()).unwrap();
    assert_eq!(
        *runtime.entry(run).unwrap().phase_rx.borrow(),
        AgentRunPhase::Waiting
    );
    assert_eq!(
        runtime.entry(run).unwrap().inbox_tx.capacity(),
        INBOX_CAPACITY
    );
    assert_eq!(
        runtime.entry(run).unwrap().config.model_preference,
        before_config.model_preference
    );
    let mut events = runtime.shared.bus.subscribe();
    runtime
        .continue_goal(run, "real followup".into(), RunConfig::default())
        .unwrap();
    // Observe the next completed turn, not the old Waiting value.
    loop {
        if matches!(events.recv().await.unwrap().kind,
            event_bus::EventKind::Lifecycle(LifecycleEvent::TurnCompleted { run_id, .. })
            if run_id == run.to_string())
        {
            break;
        }
    }
    {
        let requests = model.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        let saved: Vec<Message> = serde_json::from_str(&before.messages_json).unwrap();
        assert_eq!(&requests[1][..saved.len()], saved.as_slice());
        assert_eq!(requests[1].len(), saved.len() + 1);
        assert_eq!(
            requests[1].last().unwrap().content,
            vec![ContentBlock::Text {
                text: "real followup".into()
            }]
        );
    }
    runtime.stop(run, StopScope::SelfOnly).unwrap();
    runtime.wait(run).await.unwrap();
}

#[tokio::test]
async fn unfinished_response_resumes_without_ledger_memory_or_new_review_evidence() {
    for finish in [
        FinishReason::Length,
        FinishReason::ContentFilter,
        FinishReason::Other("interrupted".into()),
    ] {
        let model = Arc::new(RecordingModel {
            requests: Mutex::new(Vec::new()),
            first_finish: finish,
        });
        let fixture = Fixture::with_model(model.clone());
        let runtime = &fixture.runtime;
        let run = runtime
            .delegate_chat(
                "thread",
                Role::Worker,
                "original request".into(),
                RunConfig::default(),
            )
            .unwrap();
        assert_eq!(runtime.wait(run).await.unwrap(), AgentRunPhase::Error);
        let before = record(&fixture, run);
        fixture
            .storage
            .handle()
            .append_run_ledger(&run.to_string(), "a newer ledger entry")
            .unwrap();
        fixture
            .storage
            .handle()
            .append_lesson(&storage::memory::Lesson {
                id: "new-lesson".into(),
                project: "project".into(),
                task_id: "task".into(),
                content: "a newer memory lesson".into(),
                evidence: "test".into(),
            })
            .unwrap();
        fixture
            .storage
            .handle()
            .validate_lesson("new-lesson", "test")
            .unwrap();
        fixture
            .storage
            .handle()
            .promote_lesson("new-lesson")
            .unwrap();
        let config = storage::StorageConfig {
            db_path: fixture._dir.path().join("goal.sqlite3"),
            ..Default::default()
        };
        let bus = Arc::new(EventBus::new(128));
        let fresh = AgentRuntime::new(bus.clone(), Arc::new(ToolExecutor::new(bus)), model.clone())
            .with_run_store(crate::RunStore::open(&config, fixture.storage.handle()).unwrap());
        fresh
            .resume_chat(
                run,
                RunConfig {
                    memory: Some(
                        crate::memory::MemoryBoundary::capture(&config, "project").unwrap(),
                    ),
                    ..Default::default()
                },
            )
            .unwrap();
        wait_for_waiting(&fresh, run).await;
        {
            let requests = model.requests.lock().unwrap();
            let saved: Vec<Message> = serde_json::from_str(&before.messages_json).unwrap();
            assert_eq!(requests.len(), 2);
            assert_eq!(requests[1], saved);
        }
        let review = fresh.shared.review_runs.lock().unwrap()[&run].clone();
        assert!(review.user_requests.lock().unwrap().is_empty());
        assert!(fresh.trusted_thread_request(run).is_none());
        fresh.stop(run, StopScope::SelfOnly).unwrap();
        fresh.wait(run).await.unwrap();
    }
}

#[tokio::test]
async fn pending_escalation_resume_preserves_seed_and_later_human_evidence() {
    let model = Arc::new(super::registration::AdmissionModel {
        reject: AtomicBool::new(false),
    });
    let fixture = Fixture::with_model(model.clone());
    let runtime = &fixture.runtime;
    let source = runtime
        .delegate_chat(
            "source",
            Role::Worker,
            "trusted original".into(),
            RunConfig::default(),
        )
        .unwrap();
    runtime.wait(source).await.unwrap();
    let target = runtime.shared.run_ids.next();
    crate::restore::persist_escalation_seed(
        runtime,
        target,
        source,
        "worker memo",
        &RunConfig::default(),
        None,
        false,
    )
    .unwrap();
    let before = record(&fixture, target);
    model.reject.store(true, Ordering::Relaxed);
    runtime.resume_chat(target, RunConfig::default()).unwrap();
    assert!(runtime.wait_admission(target).await.is_err());
    let after = record(&fixture, target);
    assert_eq!(after.messages_json, before.messages_json);
    let descriptor: RunRestoreDescriptor = serde_json::from_str(&after.config_json).unwrap();
    assert_eq!(
        descriptor
            .pending_escalation
            .unwrap()
            .trusted_request
            .as_deref(),
        Some("trusted original")
    );
    assert_eq!(
        runtime.trusted_thread_request(target).as_deref(),
        Some("trusted original")
    );
    model.reject.store(false, Ordering::Relaxed);
    runtime.resume_chat(target, RunConfig::default()).unwrap();
    runtime.wait_admission(target).await.unwrap();
    wait_for_waiting(runtime, target).await;
    let review = runtime.shared.review_runs.lock().unwrap()[&target].clone();
    assert_eq!(
        review.lineage_run_ids,
        vec![source.to_string(), target.to_string()]
    );
    assert!(review.delegation_chain.is_empty());
    runtime
        .continue_goal(target, "real followup".into(), RunConfig::default())
        .unwrap();
    let question = runtime
        .request_user_question(target, "Allow publishing?".into(), vec![], false)
        .unwrap();
    runtime
        .answer_user_question(&question.id, "yes, publish")
        .unwrap();
    let evidence = serde_json::to_value(review.requests()).unwrap();
    assert_eq!(
        evidence,
        serde_json::json!([
            {"target_run_id": source.to_string(), "text": "trusted original"},
            {"target_run_id": target.to_string(), "text": "real followup"},
            {"target_run_id": target.to_string(), "text": "yes, publish",
                "in_reply_to": {"id": question.id, "title": question.title}}
        ])
    );
    runtime.stop(target, StopScope::SelfOnly).unwrap();
    runtime.wait(target).await.unwrap();
}
