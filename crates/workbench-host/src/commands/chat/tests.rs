use super::*;
use crate::commands::{ContinueInput, HostCommand};
use event_bus::{AgentRunPhase, EventBus, EventKind, EventReceiver, LifecycleEvent};
use providers::{ChatResponse, ContentBlock, FinishReason, Message, ToolSpec};
use runtime::{AgentInvocationContext, AgentModel, AgentRuntime, RunStore, RuntimeError};
use std::sync::Arc;

struct IdleModel;
#[async_trait::async_trait]
impl AgentModel for IdleModel {
    async fn complete(
        &self,
        _: &AgentInvocationContext,
        _: Role,
        _: &[Message],
        _: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        Ok(ChatResponse {
            usage_reported: true,
            message: Message {
                role: providers::Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "idle".into(),
                }],
            },
            usage: Default::default(),
            finish_reason: FinishReason::Stop,
        })
    }
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "idle-test".into()
    }
}
async fn waiting(events: &mut EventReceiver, run: RunId) {
    loop {
        if matches!(events.recv().await.unwrap().kind, EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged { run_id, to: AgentRunPhase::Waiting, .. }) if run_id == run.to_string())
        {
            return;
        }
    }
}
fn continuation(automatic: bool) -> HostCommand {
    HostCommand::ContinueChat(ContinueInput {
        project_root: None,
        thread_id: "stop-pending".into(),
        composer_role: ComposerRole::Orchestrator,
        model_preference: None,
        automatic,
    })
}
struct ChatFixture {
    _dir: tempfile::TempDir,
    _storage: storage::Storage,
    runtime: AgentRuntime,
    service: CommandService,
    run: RunId,
    events: EventReceiver,
}
async fn chat_fixture() -> ChatFixture {
    let dir = tempfile::tempdir().unwrap();
    let config = storage::StorageConfig {
        db_path: dir.path().join("events.db"),
        ..Default::default()
    };
    let storage = storage::Storage::open(config.clone()).unwrap();
    let bus = Arc::new(EventBus::new(1024));
    let mut events = bus.subscribe();
    let runtime = AgentRuntime::new(
        bus.clone(),
        Arc::new(tools::ToolExecutor::new(bus)),
        Arc::new(IdleModel),
    )
    .with_run_store(RunStore::open(&config, storage.handle()).unwrap());
    let run = runtime
        .delegate_chat(
            "stop-pending",
            Role::Orchestrator,
            "coordinate".into(),
            RunConfig {
                interactive: true,
                keep_alive: true,
                conversation: true,
                ..Default::default()
            },
        )
        .unwrap();
    let mut service = CommandService::new(runtime.clone(), tokio::runtime::Handle::current());
    service
        .bindings
        .chat_runs
        .insert("stop-pending".into(), run);
    waiting(&mut events, run).await;
    ChatFixture {
        _dir: dir,
        _storage: storage,
        runtime,
        service,
        run,
        events,
    }
}

thread_local! { static AFTER_RESUME_GUARD: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) }; }
pub(super) fn after_resume_guard() {
    let hook = AFTER_RESUME_GUARD.with(|hook| hook.borrow_mut().take());
    if let Some(hook) = hook {
        hook();
    }
}

#[tokio::test]
async fn stop_pending_continue_reaches_runtime_but_ordinary_live_continue_is_not_applied() {
    let ChatFixture {
        _dir,
        _storage,
        runtime,
        mut service,
        run,
        mut events,
    } = chat_fixture().await;
    assert!(!runtime.graceful_stop_requested(run));
    assert!(matches!(
        service
            .submit_authorized(continuation(false), None)
            .as_slice(),
        [HostEvent::ChatNotice {
            outcome: NoticeOutcome::NotApplied,
            ..
        }]
    ));
    service.submit_authorized(
        HostCommand::StopChat {
            thread_id: "stop-pending".into(),
        },
        None,
    );
    // No await: Stop is recorded, but this executor cannot retire the old run yet.
    assert_eq!(
        runtime.inspect_agent(run).unwrap().phase,
        AgentRunPhase::Waiting
    );
    assert!(runtime.graceful_stop_requested(run));
    assert!(matches!(
        service
            .submit_authorized(continuation(true), None)
            .as_slice(),
        [HostEvent::ChatNotice {
            outcome: NoticeOutcome::NotApplied,
            ..
        }]
    ));
    let accepted = service.submit_authorized(continuation(false), None);
    assert!(
        matches!(accepted.as_slice(), [HostEvent::ChatAccepted { run_id, .. }] if run_id == &run.to_string()),
        "graceful Stop-pending Continue must reach runtime: {accepted:?}"
    );
    assert_eq!(runtime.wait(run).await.unwrap(), AgentRunPhase::Stopped);
    waiting(&mut events, run).await;
    assert!(!runtime.graceful_stop_requested(run));
    assert!(matches!(
        service
            .submit_authorized(continuation(false), None)
            .as_slice(),
        [HostEvent::ChatNotice {
            outcome: NoticeOutcome::NotApplied,
            ..
        }]
    ));
    runtime.stop(run, runtime::StopScope::Subtree).unwrap();
    runtime.wait(run).await.unwrap();
}

#[tokio::test]
async fn resume_race_cancel_after_host_guard_rejects_old_continue_but_accepts_fresh() {
    let ChatFixture {
        _dir,
        _storage,
        runtime,
        mut service,
        run,
        mut events,
    } = chat_fixture().await;
    let original = runtime
        .resume_intents()
        .unwrap()
        .into_iter()
        .find(|i| i.run_id == run)
        .unwrap()
        .incarnation;
    runtime.stop(run, runtime::StopScope::SelfOnly).unwrap();
    let guarded = Arc::new(std::sync::Barrier::new(2));
    let release = Arc::new(std::sync::Barrier::new(2));
    let (result_tx, result_rx) = tokio::sync::oneshot::channel();
    let handle = tokio::runtime::Handle::current();
    let worker_guarded = guarded.clone();
    let worker_release = release.clone();
    let submission = std::thread::spawn(move || {
        let _enter = handle.enter();
        AFTER_RESUME_GUARD.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || {
                worker_guarded.wait();
                worker_release.wait();
            }))
        });
        let result = service.submit_authorized(continuation(false), None);
        result_tx.send((service, result)).ok().unwrap();
    });
    // Block this current-thread executor only until the host guard passes, so
    // the stopped execution cannot retire before the stale request is observed.
    guarded.wait();
    assert_eq!(
        runtime.inspect_agent(run).unwrap().phase,
        AgentRunPhase::Waiting
    );
    runtime.cancel(run).unwrap();
    assert_eq!(runtime.wait(run).await.unwrap(), AgentRunPhase::Error);
    let cancelled = runtime
        .resume_intents()
        .unwrap()
        .into_iter()
        .find(|i| i.run_id == run)
        .unwrap();
    assert!(!cancelled.active);
    assert_eq!(
        cancelled.stop_reason,
        Some(runtime::quiesce::StopReason::Cancelled)
    );
    release.wait();
    let (mut service, old_result) = result_rx.await.unwrap();
    submission.join().unwrap();
    let unchanged = runtime
        .resume_intents()
        .unwrap()
        .into_iter()
        .find(|i| i.run_id == run)
        .unwrap()
        .incarnation
        == original;
    let failed = events.drain_pending_snapshot().iter().any(
        |event| matches!(&event.kind, EventKind::Diagnostic(d) if d.code == "chat_resume_failed"),
    );
    // A genuinely newer user action observes the completed Cancel and is not
    // rejected merely because the previous incarnation was cancelled.
    let fresh_result = service.submit_authorized(continuation(false), None);
    let fresh = matches!(fresh_result.as_slice(), [HostEvent::ChatAccepted { .. }]);
    runtime.stop(run, runtime::StopScope::Subtree).unwrap();
    runtime.wait(run).await.unwrap();
    assert!(
        matches!(old_result.as_slice(), [HostEvent::ChatRejected { .. }]),
        "old Continue revoked Cancel: {old_result:?}"
    );
    assert!(unchanged, "the stale Continue created a new incarnation");
    assert!(failed, "the refusal must be observable");
    assert!(
        fresh,
        "a fresh Continue after terminal Cancel must work: {fresh_result:?}"
    );
}
