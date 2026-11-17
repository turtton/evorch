//! Exercise the actual private handlers without advertising them to the model.
mod boundaries;
mod writer;
use super::*;
use crate::{AgentInvocationContext, AgentModel, AgentRunPhase, Role, RunConfig, RuntimeError};
use providers::{ChatResponse, ContentBlock, FinishReason, Message, ToolSpec};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::Notify;

#[derive(Default)]
enum Action {
    #[default]
    Stop,
    Tool(&'static str, Value),
    Fail,
    Panic,
}
#[derive(Default)]
struct Model {
    entered: Notify,
    release: Notify,
    action: Mutex<Action>,
    calls: AtomicUsize,
    schemas: Mutex<Vec<Vec<String>>>,
    requests: Mutex<Vec<Vec<Message>>>,
}
#[async_trait::async_trait]
impl AgentModel for Model {
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "plan-test".into()
    }
    async fn complete(
        &self,
        _: &AgentInvocationContext,
        _: Role,
        messages: &[Message],
        specs: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        self.requests.lock().unwrap().push(messages.to_vec());
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.schemas
            .lock()
            .unwrap()
            .push(specs.iter().map(|s| s.name.clone()).collect());
        self.entered.notify_one();
        self.release.notified().await;
        let action = std::mem::take(&mut *self.action.lock().unwrap());
        let (content, reason) = match action {
            Action::Stop => (
                vec![ContentBlock::Text {
                    text: "done".into(),
                }],
                FinishReason::Stop,
            ),
            Action::Tool(name, input) => (
                vec![ContentBlock::ToolUse {
                    id: format!("call-{}", self.calls.load(Ordering::SeqCst)),
                    name: name.into(),
                    input,
                }],
                FinishReason::ToolUse,
            ),
            Action::Fail => {
                return Err(RuntimeError::UnknownRun {
                    run_id: "fixture failure".into(),
                });
            }
            Action::Panic => panic!("plan fixture panic"),
        };
        Ok(ChatResponse {
            message: Message {
                role: providers::Role::Assistant,
                content,
            },
            finish_reason: reason,
            usage: Default::default(),
            usage_reported: true,
        })
    }
}
struct Fixture {
    dir: tempfile::TempDir,
    storage: storage::Storage,
    config: storage::StorageConfig,
    runtime: AgentRuntime,
    model: Arc<Model>,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let config = storage::StorageConfig {
            db_path: dir.path().join("plan.db"),
            ..Default::default()
        };
        let storage = storage::Storage::open(config.clone()).unwrap();
        let model = Arc::new(Model::default());
        let runtime = Self::runtime(&config, &storage, model.clone());
        Self {
            dir,
            storage,
            config,
            runtime,
            model,
        }
    }
    fn runtime(
        config: &storage::StorageConfig,
        storage: &storage::Storage,
        model: Arc<Model>,
    ) -> AgentRuntime {
        let bus = Arc::new(event_bus::EventBus::new(4096));
        AgentRuntime::new(
            bus.clone(),
            Arc::new(::tools::ToolExecutor::new(bus)),
            model,
        )
        .with_run_store(crate::RunStore::open(config, storage.handle()).unwrap())
    }
    async fn start(&self, thread: &str) -> RunId {
        self.start_config(thread, RunConfig::default()).await
    }
    async fn start_config(&self, thread: &str, config: RunConfig) -> RunId {
        self.start_role(thread, Role::Worker, config).await
    }
    async fn start_role(&self, thread: &str, role: Role, mut config: RunConfig) -> RunId {
        config.conversation = true;
        let run = self
            .runtime
            .delegate_chat(thread, role, "work".into(), config)
            .unwrap();
        self.model.entered.notified().await;
        run
    }
    fn mailbox(&self, run: RunId) -> Arc<crate::mailbox::RunMailbox> {
        self.runtime.run_mailbox(run).unwrap()
    }
    fn caller<'a>(&self, run: RunId, mailbox: &'a Arc<crate::mailbox::RunMailbox>) -> Caller<'a> {
        Caller {
            run,
            mailbox,
            permit: None,
        }
    }
    fn add(&self, run: RunId) -> Value {
        tools::update(
            &self.runtime,
            &self.caller(run, &self.mailbox(run)),
            addition(0, "in_progress"),
        )
        .unwrap()
    }
    fn respond(&self, action: Action) {
        *self.model.action.lock().unwrap() = action;
        self.model.release.notify_one();
    }
    async fn stop(&self, run: RunId) {
        self.runtime.cancel(run).unwrap();
        self.runtime.wait(run).await.unwrap();
    }
}
fn addition(revision: u64, status: &str) -> Value {
    json!({"expected_revision":revision,"operations":[{"op":"add","client_ref":"one","title":"implement","status":status}]})
}
fn read(f: &Fixture, run: RunId) -> Value {
    tools::read(
        &f.runtime,
        &f.caller(run, &f.mailbox(run)),
        json!({"view":"tree"}),
    )
    .unwrap()
}

#[tokio::test]
async fn private_handlers_cas_paging_and_runtime_only_status() {
    let f = Fixture::new();
    let run = f.start("thread").await;
    let mailbox = f.mailbox(run);
    let caller = f.caller(run, &mailbox);
    assert_eq!(read(&f, run)["revision"], 0);
    let first = f.add(run);
    let task = first["created"][0][1].as_str().unwrap();
    assert_eq!(first["revision"], 1);
    assert!(first["progress"]["weighted_ppm"]["completed"] == 0);
    for invalid in [
        addition(0, "pending"),
        addition(1, "interrupted"),
        json!({"thread_id":"other","expected_revision":1,"operations":[]}),
        json!({"expected_revision":1,"operations":[{"op":"clear","typo":true}]}),
    ] {
        assert!(tools::update(&f.runtime, &caller, invalid).is_err());
    }
    let detail = tools::read(&f.runtime, &caller, json!({"view":"task","task_id":task})).unwrap();
    assert_eq!(detail["node"]["status"], "in_progress");
    assert!(detail.to_string().len() < 16 * 1024);
    assert!(
        tools::read(
            &f.runtime,
            &caller,
            json!({"view":"task","task_id":task,"cursor":"bad"})
        )
        .is_err()
    );
    let more = tools::update(&f.runtime, &caller, addition(1, "pending")).unwrap();
    assert_eq!(more["revision"], 2);
    let page = tools::read(&f.runtime, &caller, json!({"view":"tree","limit":1})).unwrap();
    let cursor = page["next_cursor"].clone();
    assert!(cursor.is_string());
    let noop=tools::update(&f.runtime,&caller,json!({"expected_revision":2,"operations":[{"op":"update","task":{"id":task},"title":"implement"}]})).unwrap();
    assert_eq!(noop["changed"], false);
    assert_eq!(noop["revision"], 2);
    tools::update(
        &f.runtime,
        &caller,
        json!({"expected_revision":2,"operations":[{"op":"clear"}]}),
    )
    .unwrap();
    assert!(tools::read(&f.runtime, &caller, json!({"view":"tree","cursor":cursor})).is_err());
    assert_eq!(read(&f, run)["revision"], 3);
    assert_eq!(read(&f, run)["nodes"], json!([]));
    {
        let schemas = f.model.schemas.lock().unwrap();
        assert!(schemas[0].contains(&"plan_read".into()));
        assert!(schemas[0].contains(&"plan_update".into()));
        assert!(!schemas[0].contains(&"todo_write".into()));
    }
    f.stop(run).await;
}

#[tokio::test]
async fn parallel_calls_commit_only_one_revision_and_one_saved_event() {
    let f = Fixture::new();
    let run = f.start("thread").await;
    let results = std::thread::scope(|scope| {
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let calls: Vec<_> = (0..2)
            .map(|_| {
                let barrier = barrier.clone();
                let rt = &f.runtime;
                let mailbox = f.mailbox(run);
                scope.spawn(move || {
                    barrier.wait();
                    tools::update(
                        rt,
                        &Caller {
                            run,
                            mailbox: &mailbox,
                            permit: None,
                        },
                        addition(0, "pending"),
                    )
                })
            })
            .collect();
        calls
            .into_iter()
            .map(|c| c.join().unwrap())
            .collect::<Vec<_>>()
    });
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    let h = f
        .runtime
        .conversation_plan_history("thread", None, None)
        .unwrap();
    assert_eq!(h.entries.len(), 1);
    assert_eq!(h.revision, 1);
    f.stop(run).await;
}

#[tokio::test]
async fn superseded_root_child_foreign_thread_and_reused_execution_are_refused() {
    let f = Fixture::new();
    let old = f
        .start_config(
            "thread",
            RunConfig {
                interactive: true,
                keep_alive: true,
                ..Default::default()
            },
        )
        .await;
    f.add(old);
    let old_mailbox = f.mailbox(old);
    let new = f.runtime.reserve_run_id();
    f.runtime.bind_thread_root("thread", new).unwrap();
    let caller = f.caller(old, &old_mailbox);
    assert!(tools::update(&f.runtime, &caller, addition(1, "pending")).is_err());
    f.runtime.bind_thread_root("thread", old).unwrap();
    let wrong = Arc::new(crate::mailbox::RunMailbox::new());
    assert!(tools::read(&f.runtime, &f.caller(old, &wrong), json!({"view":"tree"})).is_err());
    let child = f.runtime.spawn_reserved(
        f.runtime.reserve_run_id(),
        Some(old),
        Role::Worker,
        "child",
        RunConfig {
            conversation: true,
            ..Default::default()
        },
    );
    f.model.entered.notified().await;
    assert!(
        tools::read(
            &f.runtime,
            &f.caller(child, &f.mailbox(child)),
            json!({"view":"tree"})
        )
        .is_err()
    );
    let other = f.start("other").await;
    let other_task = f.add(other)["created"][0][1].as_str().unwrap().to_owned();
    assert!(
        tools::read(
            &f.runtime,
            &caller,
            json!({"view":"task","task_id":other_task})
        )
        .is_err()
    );
    assert!(!f.runtime.reconcile_plan_or_report(&f.caller(other, &wrong)));
    assert_eq!(read(&f, old)["revision"], 1);
    f.stop(child).await;
    f.stop(other).await;
    f.stop(old).await;
    // A validated conversation restore starts a new generation, unlike a fresh
    // spawn which correctly inherits the old execution's cancellation fence.
    let reused = f
        .runtime
        .resume_chat(
            old,
            RunConfig {
                conversation: true,
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(reused, old);
    f.model.entered.notified().await;
    assert!(!Arc::ptr_eq(&old_mailbox, &f.mailbox(old)));
    assert!(tools::update(&f.runtime, &caller, addition(2, "pending")).is_err());
    tools::read(
        &f.runtime,
        &f.caller(old, &f.mailbox(old)),
        json!({"view":"tree"}),
    )
    .expect("new root execution may read the same plan");
    f.stop(old).await;
}

#[tokio::test]
async fn owner_generation_and_thread_binding_are_checked_for_reads_and_writes() {
    let f = Fixture::new();
    let path = f.dir.path().join("owners.db");
    let mut registry = crate::ownership::Registry::open(&path).unwrap();
    let owner = crate::ownership::ThreadOwner::new(
        "thread".into(),
        crate::ownership::Lease {
            owner_id: "host".into(),
            generation: 1,
            expires_at: u64::MAX,
        },
    );
    registry.start(&owner).unwrap();
    let permit = OwnerPermit {
        registry_path: path,
        thread_id: "thread".into(),
        lease: owner.lease.clone(),
        run_id: None,
    };
    let run = f
        .start_config(
            "thread",
            RunConfig {
                ownership: Some(permit),
                ..Default::default()
            },
        )
        .await;
    let permit = f
        .runtime
        .shared
        .runs
        .lock()
        .unwrap()
        .get(&run)
        .unwrap()
        .config
        .ownership
        .clone()
        .unwrap();
    let mailbox = f.mailbox(run);
    let caller = Caller {
        run,
        mailbox: &mailbox,
        permit: Some(&permit),
    };
    tools::update(&f.runtime, &caller, addition(0, "in_progress")).unwrap();
    let mut next = registry.attach("thread").unwrap();
    next.lease.generation += 1;
    registry
        .update("thread", |owner| {
            *owner = next;
            Ok(())
        })
        .unwrap();
    assert!(tools::read(&f.runtime, &caller, json!({"view":"tree"})).is_err());
    assert!(tools::update(&f.runtime, &caller, addition(1, "pending")).is_err());
    assert!(!f.runtime.reconcile_plan_or_report(&caller));
    assert_eq!(
        f.runtime
            .conversation_plan_summary("thread")
            .unwrap()
            .revision,
        1
    );
    f.stop(run).await;
}

#[tokio::test]
async fn completed_turn_reconciles_once_and_does_not_complete_goal_or_wake() {
    let f = Fixture::new();
    let run = f
        .start_config(
            "thread",
            RunConfig {
                interactive: true,
                keep_alive: true,
                ..Default::default()
            },
        )
        .await;
    let goal = f
        .runtime
        .create_thread_goal("thread", run, "objective".into(), vec!["criterion".into()])
        .unwrap();
    f.add(run);
    assert_eq!(f.runtime.thread_goal("thread").unwrap().usage, goal.usage);
    f.runtime
        .set_goal_checks_paused("thread", &goal.goal_id, true)
        .unwrap();
    let mut events = f.runtime.event_bus().subscribe();
    f.respond(Action::Stop);
    loop {
        if let event_bus::EventKind::Lifecycle(event_bus::LifecycleEvent::TurnCompleted {
            run_id,
            ..
        }) = events.recv().await.unwrap().kind
            && run_id == run.to_string()
        {
            break;
        }
    }
    let p = f.runtime.conversation_plan_summary("thread").unwrap();
    assert_eq!(p.revision, 2);
    assert_eq!(p.progress.leaf_counts.interrupted, 1);
    assert!(
        !f.runtime
            .reconcile_plan_or_report(&f.caller(run, &f.mailbox(run)))
    );
    let current = f.runtime.thread_goal("thread").unwrap();
    assert_eq!(current.goal_id, goal.goal_id);
    assert_ne!(current.phase, event_bus::ThreadGoalPhase::Complete);
    assert_eq!(current.usage.model_requests, 0);
    assert_eq!(f.model.calls.load(Ordering::SeqCst), 1);
    f.stop(run).await;
}

#[tokio::test]
async fn terminal_boundaries_reconcile_and_failure_is_only_diagnostic() {
    for action in [Action::Stop, Action::Fail, Action::Panic] {
        let f = Fixture::new();
        let run = f.start("thread").await;
        f.add(run);
        let expected = if matches!(action, Action::Stop) {
            AgentRunPhase::Done
        } else {
            AgentRunPhase::Error
        };
        f.respond(action);
        f.runtime.wait(run).await.unwrap();
        assert_eq!(f.runtime.inspect_agent(run).unwrap().phase, expected);
        let p = f.runtime.conversation_plan_summary("thread").unwrap();
        assert_eq!(p.revision, 2);
        assert_eq!(p.progress.leaf_counts.interrupted, 1);
    }
    let f = Fixture::new();
    let run = f.start("thread").await;
    f.add(run);
    let conn = rusqlite::Connection::open(&f.config.db_path).unwrap();
    conn.execute_batch("CREATE TRIGGER fail_plan_change BEFORE INSERT ON events WHEN NEW.kind='ConversationPlan' BEGIN SELECT RAISE(ABORT, 'fixture'); END;").unwrap();
    let mut events = f.runtime.event_bus().subscribe();
    f.respond(Action::Stop);
    f.runtime.wait(run).await.unwrap();
    assert_eq!(
        f.runtime.inspect_agent(run).unwrap().phase,
        AgentRunPhase::Done
    );
    assert_eq!(
        f.runtime
            .conversation_plan_summary("thread")
            .unwrap()
            .revision,
        1
    );
    loop {
        if let event_bus::EventKind::Diagnostic(d) = events.recv().await.unwrap().kind
            && d.code == "conversation_plan_reconcile_failed"
        {
            break;
        }
    }
}

#[tokio::test]
async fn blocking_question_and_refused_finish_do_not_interrupt() {
    let f = Fixture::new();
    let run = f
        .start_role(
            "thread",
            Role::Orchestrator,
            RunConfig {
                interactive: true,
                keep_alive: true,
                ..Default::default()
            },
        )
        .await;
    f.add(run);
    f.respond(Action::Tool(
        "ask_user",
        json!({"title":"choose","blocking":true}),
    ));
    f.model.entered.notified().await;
    assert_eq!(
        f.runtime
            .conversation_plan_summary("thread")
            .unwrap()
            .revision,
        1
    );
    assert!(f.runtime.has_pending_user_questions(run));
    f.respond(Action::Tool("finish", json!({"result":"premature"})));
    f.model.entered.notified().await;
    assert_eq!(
        f.runtime
            .conversation_plan_summary("thread")
            .unwrap()
            .revision,
        1
    );
    let mut events = f.runtime.event_bus().subscribe();
    f.respond(Action::Stop);
    loop {
        if let event_bus::EventKind::Lifecycle(event_bus::LifecycleEvent::TurnCompleted {
            run_id,
            ..
        }) = events.recv().await.unwrap().kind
            && run_id == run.to_string()
        {
            break;
        }
    }
    assert_eq!(
        f.runtime
            .conversation_plan_summary("thread")
            .unwrap()
            .revision,
        1
    );
    f.stop(run).await;
    assert_eq!(
        f.runtime
            .conversation_plan_summary("thread")
            .unwrap()
            .progress
            .leaf_counts
            .interrupted,
        1
    );
}

#[tokio::test]
async fn projection_survives_runtime_restart_and_handoff_without_snapshot_event() {
    let f = Fixture::new();
    let source = f.start("thread").await;
    f.add(source);
    let target = f.runtime.reserve_run_id();
    let child = event_bus::escalation_thread_id(&target.to_string());
    f.runtime
        .handoff_thread_goal(source, target, None, |_| {})
        .unwrap();
    assert_eq!(
        f.runtime
            .conversation_plan_summary("thread")
            .unwrap()
            .revision,
        0
    );
    assert_eq!(
        f.runtime
            .conversation_plan_summary(&child)
            .unwrap()
            .revision,
        1
    );
    assert_eq!(
        f.runtime
            .conversation_plan_history(&child, None, None)
            .unwrap()
            .entries
            .len(),
        1
    );
    assert!(
        !f.runtime
            .reconcile_plan_or_report(&f.caller(source, &f.mailbox(source)))
    );
    let restarted = Fixture::runtime(&f.config, &f.storage, Arc::new(Model::default()));
    assert_eq!(
        restarted
            .conversation_plan_summary(&child)
            .unwrap()
            .progress
            .leaf_counts
            .in_progress,
        1
    );
    assert_eq!(
        restarted
            .conversation_plan_summary(&child)
            .unwrap()
            .revision,
        1
    );
    f.stop(source).await;
}

#[tokio::test]
async fn fork_and_rewind_history_seeds_start_unplanned_without_changing_source() {
    let f = Fixture::new();
    let source = f
        .start_config(
            "source",
            RunConfig {
                interactive: true,
                keep_alive: true,
                ..Default::default()
            },
        )
        .await;
    f.add(source);
    let mut events = f.runtime.event_bus().subscribe();
    f.respond(Action::Stop);
    let boundary = loop {
        if let event_bus::EventKind::Lifecycle(event_bus::LifecycleEvent::TurnCompleted {
            run_id,
            context_len,
        }) = events.recv().await.unwrap().kind
            && run_id == source.to_string()
        {
            break context_len;
        }
    };
    let revision = f
        .runtime
        .conversation_plan_summary("source")
        .unwrap()
        .revision;
    for destination in ["fork-destination", "rewind-destination"] {
        let run = f
            .runtime
            .delegate_chat_seeded(
                destination,
                Role::Worker,
                "branch".into(),
                RunConfig {
                    conversation: true,
                    ..Default::default()
                },
                Some(crate::ChatForkSeed {
                    source_run_id: source.to_string(),
                    context_len: boundary,
                }),
            )
            .unwrap();
        f.model.entered.notified().await;
        assert_eq!(read(&f, run)["revision"], 0);
        f.add(run);
        assert_eq!(
            f.runtime
                .conversation_plan_summary("source")
                .unwrap()
                .revision,
            revision
        );
        f.stop(run).await;
    }
    f.stop(source).await;
}

#[tokio::test]
async fn plan_tools_do_not_invalidate_goal_checks_or_acceptance() {
    let f = Fixture::new();
    let run = f.start("thread").await;
    f.runtime
        .create_thread_goal("thread", run, "objective".into(), vec!["criterion".into()])
        .unwrap();
    f.respond(Action::Stop);
    f.model.entered.notified().await; // The real goal boundary requested its self-check.
    let checking = f.runtime.thread_goal("thread").unwrap();
    f.respond(Action::Tool("submit_goal_check",json!({"epoch":checking.epoch,"checks":[{"criterion":0,"met":true,"evidence":"fixture evidence"}]})));
    f.model.entered.notified().await;
    let accepted = f.runtime.thread_goal("thread").unwrap();
    assert_eq!(accepted.checks.len(), 1);
    f.add(run);
    for name in ["plan_read", "plan_update"] {
        f.runtime.goal_tool_activity(run, name);
    }
    let after = f.runtime.thread_goal("thread").unwrap();
    assert_eq!(after.checks, accepted.checks);
    assert_eq!(after.epoch, accepted.epoch);
    assert_eq!(after.usage, accepted.usage);
    // The setup is non-vacuous: an output-affecting tool would invalidate it.
    f.runtime.goal_tool_activity(run, "write");
    assert!(f.runtime.thread_goal("thread").unwrap().checks.is_empty());
    f.stop(run).await;
}

#[tokio::test]
async fn accepted_finish_reconciles_without_changing_the_result() {
    let f = Fixture::new();
    let run = f
        .start_role("thread", Role::Orchestrator, RunConfig::default())
        .await;
    f.add(run);
    f.respond(Action::Tool("finish", json!({"result":"accepted answer"})));
    let phase = tokio::select! {
        result=f.runtime.wait(run)=>result.unwrap(),
        _=f.model.entered.notified()=>panic!("finish unexpectedly continued: {:?}", f.model.requests.lock().unwrap().last()),
    };
    assert_eq!(phase, AgentRunPhase::Done);
    assert_eq!(
        f.runtime.run_result(run).unwrap().as_deref(),
        Some("accepted answer")
    );
    let plan = f.runtime.conversation_plan_summary("thread").unwrap();
    assert_eq!(plan.revision, 2);
    assert_eq!(plan.progress.leaf_counts.interrupted, 1);
}
