mod support;
use event_bus::{AgentRunPhase, EventBus, EventKind, LifecycleEvent, ToolEvent};
use providers::FinishReason;
use runtime::{AgentRuntime, Role, RunConfig, RunStore};
use std::sync::Arc;
use storage::{Storage, StorageConfig};
use support::{ScriptedModel, text_response, tool_response};
use tools::ToolExecutor;

#[tokio::test]
async fn question_yields_runs_independent_work_then_wakes_once_with_free_text() {
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("questions.db"),
        ..Default::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let bus = Arc::new(EventBus::new(256));
    let mut events = bus.subscribe();
    let model = Arc::new(ScriptedModel::new([
        Ok(tool_response(
            "ask",
            "ask_user",
            serde_json::json!({"title":"どの方式？","options":["A","B"]}),
        )),
        Ok(tool_response(
            "independent",
            "user_answers",
            serde_json::json!({}),
        )),
        Ok(text_response(
            "Independent work finished; waiting.",
            FinishReason::Stop,
        )),
        Ok(text_response("Applied the answer.", FinishReason::Stop)),
    ]));
    let runtime = AgentRuntime::new(bus.clone(), Arc::new(ToolExecutor::new(bus)), model.clone())
        .with_run_store(RunStore::open(&config, storage.handle()).unwrap());
    let run = runtime.delegate_background(Role::Worker, "work".into(), RunConfig::default());
    let (mut question, mut continued) = (None, false);
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            match events.recv().await.unwrap().kind {
                EventKind::Tool(ToolEvent::UserQuestionUpdated { question: q }) => {
                    question = Some(q)
                }
                EventKind::Lifecycle(LifecycleEvent::RunProgress {
                    activity: event_bus::RunActivity::Model,
                    ..
                }) if question.is_some() => continued = true,
                EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
                    to: AgentRunPhase::Waiting,
                    ..
                }) => break,
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    assert!(
        continued,
        "the model must continue after asking without polling the user"
    );
    assert_eq!(
        runtime.inspect_agent(run).unwrap().phase,
        AgentRunPhase::Waiting
    );
    let question = question.unwrap();
    assert_eq!(
        storage::Database::open(&config)
            .unwrap()
            .pending_user_questions()
            .unwrap()
            .len(),
        1
    );
    runtime
        .answer_user_question(&question.id, "C: 自由回答")
        .unwrap();
    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(10), runtime.wait(run))
            .await
            .unwrap()
            .unwrap(),
        AgentRunPhase::Done
    );
    let observed = model.observed().await;
    let last = serde_json::to_string(observed.last().unwrap()).unwrap();
    assert!(last.contains("C: 自由回答"));
    assert_eq!(last.matches("[user-answer id=").count(), 1);
    runtime
        .answer_user_question(&question.id, "C: 自由回答")
        .unwrap();
    assert!(
        runtime
            .answer_user_question(&question.id, "different")
            .is_err()
    );
}

#[tokio::test]
async fn pending_question_survives_cancel_and_storage_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("questions.db"),
        ..Default::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let bus = Arc::new(EventBus::new(256));
    let mut events = bus.subscribe();
    let model = Arc::new(ScriptedModel::new([
        Ok(tool_response(
            "ask",
            "ask_user",
            serde_json::json!({"title":"Confirm scope"}),
        )),
        Ok(text_response("waiting", FinishReason::Stop)),
    ]));
    let runtime = AgentRuntime::new(bus.clone(), Arc::new(ToolExecutor::new(bus)), model)
        .with_run_store(RunStore::open(&config, storage.handle()).unwrap());
    let run = runtime.delegate_background(Role::Worker, "work".into(), RunConfig::default());
    let question = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if let EventKind::Tool(ToolEvent::UserQuestionUpdated { question }) =
                events.recv().await.unwrap().kind
            {
                break question;
            }
        }
    })
    .await
    .unwrap();
    runtime.cancel(run).unwrap();
    assert_eq!(runtime.wait(run).await.unwrap(), AgentRunPhase::Error);
    drop(runtime);
    drop(storage);
    let storage = Storage::open(config.clone()).unwrap();
    let db = storage::Database::open(&config).unwrap();
    assert_eq!(db.pending_user_questions().unwrap(), vec![question.clone()]);
    storage
        .handle()
        .answer_user_question(&question.id, "resume later")
        .unwrap();
    assert_eq!(
        db.user_question(&question.id)
            .unwrap()
            .unwrap()
            .answer
            .as_deref(),
        Some("resume later")
    );
}

#[tokio::test]
async fn unconfigured_question_storage_is_an_explicit_tool_error() {
    let bus = Arc::new(EventBus::new(128));
    let model = Arc::new(ScriptedModel::new([
        Ok(tool_response(
            "ask",
            "ask_user",
            serde_json::json!({"title":"scope?"}),
        )),
        Ok(text_response("storage error handled", FinishReason::Stop)),
    ]));
    let runtime = AgentRuntime::new(bus.clone(), Arc::new(ToolExecutor::new(bus)), model.clone());
    let run = runtime.delegate_background(Role::Worker, "work".into(), RunConfig::default());
    runtime.wait(run).await.unwrap();
    assert!(
        serde_json::to_string(&model.observed().await[1])
            .unwrap()
            .contains("durable question storage is not configured")
    );
}

struct FinishRaceModel {
    runtime: std::sync::Mutex<Option<AgentRuntime>>,
    db_path: std::path::PathBuf,
    fail_read: bool,
    natural_stop: bool,
    calls: std::sync::atomic::AtomicUsize,
    observed: std::sync::Mutex<Vec<Vec<providers::Message>>>,
}

#[async_trait::async_trait]
impl runtime::AgentModel for FinishRaceModel {
    async fn complete(
        &self,
        invocation: &runtime::AgentInvocationContext,
        _: Role,
        messages: &[providers::Message],
        _: &[providers::ToolSpec],
    ) -> Result<providers::ChatResponse, runtime::RuntimeError> {
        self.observed.lock().unwrap().push(messages.to_vec());
        match self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) {
            0 => Ok(tool_response(
                "ask",
                "ask_user",
                serde_json::json!({"title":"Required scope"}),
            )),
            1 => {
                // The response was computed without the answer. Publish an
                // answer (or a read failure) before returning its finish call.
                if self.fail_read {
                    rusqlite::Connection::open(&self.db_path)
                        .unwrap()
                        .execute_batch("DROP TABLE user_questions")
                        .unwrap();
                } else {
                    let runtime = self.runtime.lock().unwrap().as_ref().unwrap().clone();
                    let id = runtime::RunId::new(
                        invocation
                            .run_id
                            .strip_prefix("run-")
                            .unwrap()
                            .parse()
                            .unwrap(),
                    );
                    let question = runtime.user_answers(id).unwrap().remove(0);
                    runtime
                        .answer_user_question(&question.id, "Use option A")
                        .unwrap();
                }
                if self.natural_stop {
                    Ok(text_response(
                        "old response without user answer",
                        FinishReason::Stop,
                    ))
                } else {
                    Ok(tool_response(
                        "finish",
                        "finish",
                        serde_json::json!({"result":"old response without user answer"}),
                    ))
                }
            }
            _ => Ok(text_response(
                "Applied the newly arrived answer",
                FinishReason::Stop,
            )),
        }
    }
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "race-fixture".into()
    }
}

async fn finish_race(
    fail_read: bool,
    natural_stop: bool,
) -> (AgentRunPhase, Vec<Vec<providers::Message>>) {
    let directory = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: directory.path().join("race.sqlite3"),
        ..Default::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let bus = Arc::new(EventBus::new(256));
    let model = Arc::new(FinishRaceModel {
        runtime: std::sync::Mutex::new(None),
        db_path: config.db_path.clone(),
        fail_read,
        natural_stop,
        calls: std::sync::atomic::AtomicUsize::new(0),
        observed: std::sync::Mutex::new(Vec::new()),
    });
    let runtime = AgentRuntime::new(bus.clone(), Arc::new(ToolExecutor::new(bus)), model.clone())
        .with_run_store(RunStore::open(&config, storage.handle()).unwrap());
    *model.runtime.lock().unwrap() = Some(runtime.clone());
    let run = runtime.delegate_background(Role::Orchestrator, "work".into(), RunConfig::default());
    let phase = tokio::time::timeout(std::time::Duration::from_secs(5), runtime.wait(run))
        .await
        .unwrap()
        .unwrap();
    *model.runtime.lock().unwrap() = None;
    let observed = model.observed.lock().unwrap().clone();
    (phase, observed)
}

#[tokio::test]
async fn finish_cannot_skip_an_answer_arriving_during_model_request() {
    let (phase, observed) = finish_race(false, false).await;
    assert_eq!(phase, AgentRunPhase::Done);
    assert_eq!(
        observed.len(),
        3,
        "finish must be rejected until a new model turn consumes the answer"
    );
    let last = serde_json::to_string(observed.last().unwrap()).unwrap();
    assert!(last.contains("Use option A"));
    assert!(last.contains("New user answers have arrived"));
    assert!(!last.contains("ADR 0002"));
    assert_eq!(last.matches("[user-answer id=").count(), 1);
}

#[tokio::test]
async fn failed_question_read_cannot_be_interpreted_as_no_pending_answers() {
    let (phase, _) = finish_race(true, false).await;
    assert_eq!(phase, AgentRunPhase::Error);
}

async fn question_waiting(events: &mut event_bus::EventReceiver, run: runtime::RunId) {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
                run_id,
                to: AgentRunPhase::Waiting,
                ..
            }) = events.recv().await.unwrap().kind
                && run_id == run.to_string()
            {
                return;
            }
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn new_chat_run_inherits_pending_question_and_receives_answer_once() {
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("inherit.sqlite3"),
        ..Default::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let bus = Arc::new(EventBus::new(256));
    let mut events = bus.subscribe();
    let model = Arc::new(ScriptedModel::new([
        Ok(tool_response(
            "ask",
            "ask_user",
            serde_json::json!({"title":"Required decision"}),
        )),
        Ok(text_response("waiting", FinishReason::Stop)),
        Ok(text_response("resumed; still waiting", FinishReason::Stop)),
        Ok(text_response("applied A", FinishReason::Stop)),
    ]));
    let runtime = AgentRuntime::new(bus.clone(), Arc::new(ToolExecutor::new(bus)), model.clone())
        .with_run_store(RunStore::open(&config, storage.handle()).unwrap());
    let old = runtime
        .delegate_chat("thread", Role::Worker, "work".into(), RunConfig::default())
        .unwrap();
    question_waiting(&mut events, old).await;
    let question = runtime.user_answers(old).unwrap().remove(0);
    runtime.cancel(old).unwrap();
    runtime.wait(old).await.unwrap();
    let resumed = runtime
        .delegate_chat(
            "thread",
            Role::Worker,
            "continue".into(),
            RunConfig::default(),
        )
        .unwrap();
    assert_ne!(old, resumed);
    question_waiting(&mut events, resumed).await;
    assert_eq!(
        runtime.user_answers(resumed).unwrap(),
        vec![question.clone()]
    );
    assert!(runtime.has_active_question_recipient(&question.id).unwrap());
    runtime.answer_user_question(&question.id, "A").unwrap();
    assert_eq!(runtime.wait(resumed).await.unwrap(), AgentRunPhase::Done);
    let messages = model.observed().await;
    let last = serde_json::to_string(messages.last().unwrap()).unwrap();
    assert_eq!(last.matches("[user-answer id=").count(), 1);
    assert_eq!(
        runtime.user_question(&question.id).unwrap().unwrap().run_id,
        old.to_string()
    );
    assert!(!runtime.has_active_question_recipient(&question.id).unwrap());
    // The next continuation already has the answer in its history and does not
    // inherit its delivery obligation again.
    model
        .add_keyed(
            "work",
            [Ok(text_response(
                "continued after answer",
                FinishReason::Stop,
            ))],
        )
        .await;
    let next = runtime
        .delegate_chat(
            "thread",
            Role::Worker,
            "continue again".into(),
            RunConfig::default(),
        )
        .unwrap();
    assert_eq!(runtime.wait(next).await.unwrap(), AgentRunPhase::Done);
    assert!(runtime.user_answers(next).unwrap().is_empty());
    let messages = model.observed().await;
    assert_eq!(
        serde_json::to_string(messages.last().unwrap())
            .unwrap()
            .matches("[user-answer id=")
            .count(),
        1
    );
}

#[tokio::test]
async fn failed_question_inheritance_refuses_to_spawn_a_new_chat_run() {
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("inherit-failure.sqlite3"),
        ..Default::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let bus = Arc::new(EventBus::new(256));
    let mut events = bus.subscribe();
    let model = Arc::new(ScriptedModel::new([
        Ok(tool_response(
            "ask",
            "ask_user",
            serde_json::json!({"title":"Required decision"}),
        )),
        Ok(text_response("waiting", FinishReason::Stop)),
    ]));
    let runtime = AgentRuntime::new(bus.clone(), Arc::new(ToolExecutor::new(bus)), model)
        .with_run_store(RunStore::open(&config, storage.handle()).unwrap());
    let old = runtime
        .delegate_chat("thread", Role::Worker, "work".into(), RunConfig::default())
        .unwrap();
    question_waiting(&mut events, old).await;
    runtime.cancel(old).unwrap();
    runtime.wait(old).await.unwrap();
    rusqlite::Connection::open(&config.db_path)
        .unwrap()
        .execute_batch("DROP TABLE user_question_links")
        .unwrap();
    let error = runtime
        .delegate_chat(
            "thread",
            Role::Worker,
            "continue".into(),
            RunConfig::default(),
        )
        .unwrap_err();
    assert!(error.to_string().contains("question inheritance failed"));
    assert_eq!(runtime.list_agents().len(), 1);
}

#[tokio::test]
async fn same_run_restore_does_not_reinject_an_answer_already_in_persisted_history() {
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("same-run.sqlite3"),
        ..Default::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let bus = Arc::new(EventBus::new(256));
    let mut events = bus.subscribe();
    let model = Arc::new(ScriptedModel::new([
        Ok(tool_response(
            "ask",
            "ask_user",
            serde_json::json!({"title":"Required scope"}),
        )),
        Ok(text_response("waiting", FinishReason::Stop)),
        Ok(tool_response(
            "finish",
            "finish",
            serde_json::json!({"result":"applied answer"}),
        )),
        Ok(tool_response(
            "finish-again",
            "finish",
            serde_json::json!({"result":"continued"}),
        )),
    ]));
    let runtime = AgentRuntime::new(bus.clone(), Arc::new(ToolExecutor::new(bus)), model.clone())
        .with_run_store(RunStore::open(&config, storage.handle()).unwrap());
    let run = runtime.delegate_background(Role::Orchestrator, "work".into(), RunConfig::default());
    question_waiting(&mut events, run).await;
    let question = runtime.user_answers(run).unwrap().remove(0);
    runtime.answer_user_question(&question.id, "A").unwrap();
    let phase = runtime.wait(run).await.unwrap();
    assert_eq!(
        phase,
        AgentRunPhase::Done,
        "observed={:?}",
        model.observed().await
    );
    assert_eq!(
        runtime
            .continue_goal(run, "continue".into(), RunConfig::default())
            .unwrap(),
        run
    );
    assert_eq!(runtime.wait(run).await.unwrap(), AgentRunPhase::Done);
    let observed = model.observed().await;
    assert_eq!(
        serde_json::to_string(observed.last().unwrap())
            .unwrap()
            .matches("[user-answer id=")
            .count(),
        1
    );
}

#[tokio::test]
async fn natural_stop_cannot_skip_an_answer_arriving_during_model_request() {
    let (phase, observed) = finish_race(false, true).await;
    assert_eq!(phase, AgentRunPhase::Done);
    assert_eq!(observed.len(), 3);
    let last = serde_json::to_string(observed.last().unwrap()).unwrap();
    assert!(last.contains("Use option A"));
    assert_eq!(last.matches("[user-answer id=").count(), 1);
}

struct PendingQuestionRecipientModel {
    admission: bool,
}
#[async_trait::async_trait]
impl runtime::AgentModel for PendingQuestionRecipientModel {
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "pending-fixture".into()
    }
    fn requires_admission(&self) -> bool {
        self.admission
    }
    async fn admit(
        &self,
        _: &runtime::AgentInvocationContext,
        _: Role,
    ) -> Result<(), runtime::RuntimeError> {
        std::future::pending().await
    }
    async fn complete(
        &self,
        _: &runtime::AgentInvocationContext,
        _: Role,
        _: &[providers::Message],
        _: &[providers::ToolSpec],
    ) -> Result<providers::ChatResponse, runtime::RuntimeError> {
        std::future::pending().await
    }
}

#[tokio::test]
async fn stale_active_question_recipient_is_fenced_even_before_provider_admission() {
    use runtime::ownership::{Lease, OwnerPermit, Registry, ThreadOwner};
    for admission in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let config = StorageConfig {
            db_path: dir.path().join("questions.db"),
            ..Default::default()
        };
        let storage = Storage::open(config.clone()).unwrap();
        let question = event_bus::UserQuestion {
            id: "question-source".into(),
            run_id: "run-1".into(),
            root_run_id: "run-1".into(),
            root_name: "chat:Worker:thread".into(),
            title: "scope".into(),
            options: vec![],
            blocking: true,
            answer: None,
        };
        storage.handle().create_user_question(&question).unwrap();
        let path = dir.path().join("owners.db");
        let owner = ThreadOwner::new(
            "thread".into(),
            Lease {
                owner_id: "first".into(),
                generation: 1,
                expires_at: 100,
            },
        );
        let mut registry = Registry::open(&path).unwrap();
        registry.start(&owner).unwrap();
        let bus = Arc::new(EventBus::new(128));
        let runtime = AgentRuntime::new(
            bus.clone(),
            Arc::new(ToolExecutor::new(bus)),
            Arc::new(PendingQuestionRecipientModel { admission }),
        )
        .with_run_store(RunStore::open(&config, storage.handle()).unwrap());
        let run = runtime.delegate_background(
            Role::Worker,
            "work".into(),
            RunConfig {
                ownership: Some(OwnerPermit {
                    registry_path: path,
                    thread_id: "thread".into(),
                    lease: owner.lease.clone(),
                    run_id: None,
                }),
                ..Default::default()
            },
        );
        storage
            .handle()
            .bind_user_questions(
                "run-1",
                &run.to_string(),
                std::slice::from_ref(&question.id),
            )
            .unwrap();
        assert!(runtime.has_active_question_recipient(&question.id).unwrap());
        registry
            .update("thread", |state| state.claim(&owner.lease, "next", 200, 50))
            .unwrap();
        let error = runtime.answer_user_question(&question.id, "A").unwrap_err();
        assert!(
            error.contains("owner or generation no longer matches"),
            "{error}"
        );
        assert!(
            runtime
                .user_question(&question.id)
                .unwrap()
                .unwrap()
                .answer
                .is_none()
        );
        runtime.cancel(run).unwrap();
    }
}
