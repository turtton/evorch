mod support;

use event_bus::{AgentRunPhase, EventBus, EventKind, LifecycleEvent};
use providers::FinishReason;
use runtime::{AgentRuntime, Role, RunConfig, RunId, RunStore};
use std::sync::Arc;
use storage::{Storage, StorageConfig};
use support::{ScriptedModel, text_response, tool_response};
use tools::ToolExecutor;

struct Fixture {
    bus: Arc<EventBus>,
    runtime: AgentRuntime,
    model: Arc<ScriptedModel>,
    parent: RunId,
    child: RunId,
    storage: Storage,
    config: StorageConfig,
    _dir: tempfile::TempDir,
}

async fn fixture(blocking: bool, owned: bool) -> Fixture {
    fixture_with_hook(blocking, owned, None).await
}

async fn fixture_with_hook(
    blocking: bool,
    owned: bool,
    hook: Option<Arc<dyn tools::post_edit::PostEditHook>>,
) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("questions.db"),
        ..Default::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let bus = Arc::new(EventBus::new(256));
    let model = Arc::new(ScriptedModel::new([]));
    model
        .gate_key("PARENT", Arc::new(tokio::sync::Notify::new()))
        .await;
    model
        .add_keyed(
            "CHILD",
            [
                Ok(tool_response(
                    "question",
                    "ask_user",
                    serde_json::json!({"title":"Which implementation?","blocking":blocking}),
                )),
                Ok(text_response(
                    "Independent work completed",
                    FinishReason::Stop,
                )),
                Ok(text_response("Applied the answer", FinishReason::Stop)),
            ],
        )
        .await;
    let runtime = AgentRuntime::new(
        bus.clone(),
        Arc::new(ToolExecutor::new(bus.clone()).with_post_edit_hook(hook)),
        model.clone(),
    )
    .with_sequential_run_ids()
    .with_run_store(RunStore::open(&config, storage.handle()).unwrap());
    let ownership = owned.then(|| {
        use runtime::ownership::{Lease, OwnerPermit, Registry, ThreadOwner};
        let registry_path = dir.path().join("owners.db");
        let owner = ThreadOwner::new(
            "thread".into(),
            Lease {
                owner_id: "first".into(),
                generation: 1,
                expires_at: u64::MAX,
            },
        );
        Registry::open(&registry_path)
            .unwrap()
            .start(&owner)
            .unwrap();
        OwnerPermit {
            registry_path,
            thread_id: owner.thread_id,
            lease: owner.lease,
            run_id: None,
        }
    });
    let parent = runtime.delegate_background(
        Role::Orchestrator,
        "PARENT".into(),
        RunConfig {
            ownership,
            ..Default::default()
        },
    );
    model.wait_for_request(0).await;
    let child = runtime
        .delegate_background_as_child(parent, Role::Worker, "CHILD", RunConfig::default())
        .unwrap();
    Fixture {
        bus,
        runtime,
        model,
        parent,
        child,
        storage,
        config,
        _dir: dir,
    }
}

#[derive(Default)]
struct FinalizationGate {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
    used: std::sync::atomic::AtomicBool,
}

#[async_trait::async_trait]
impl tools::post_edit::PostEditHook for FinalizationGate {
    async fn check(
        &self,
        _: &tools::post_edit::PostEditInput,
    ) -> tools::post_edit::PostEditOutcome {
        tools::post_edit::PostEditOutcome::Pass
    }

    async fn drain(&self) {
        if !self.used.swap(true, std::sync::atomic::Ordering::SeqCst) {
            self.entered.notify_one();
            self.release.notified().await;
        }
    }
}

#[tokio::test]
async fn parent_answer_during_natural_finalization_is_applied_once() {
    let gate = Arc::new(FinalizationGate::default());
    let fixture = fixture_with_hook(false, true, Some(gate.clone())).await;
    gate.entered.notified().await;
    assert_eq!(
        fixture.runtime.inspect_agent(fixture.child).unwrap().phase,
        AgentRunPhase::Running
    );
    let question = fixture
        .runtime
        .user_answers(fixture.child)
        .unwrap()
        .remove(0);
    let before = fixture.model.observed().await.len();
    let mut events = fixture.bus.subscribe();
    let runtime = fixture.runtime.clone();
    let parent = fixture.parent;
    let id = question.id.clone();
    let answering =
        tokio::spawn(async move { runtime.answer_subagent_question(parent, &id, "Use A").await });
    loop {
        if let EventKind::Tool(event_bus::ToolEvent::UserQuestionUpdated { question: updated }) =
            events.recv().await.unwrap().kind
            && updated.id == question.id
            && updated.answer.is_some()
        {
            break;
        }
    }
    gate.release.notify_one();
    answering.await.unwrap().unwrap();
    assert_eq!(
        fixture.runtime.wait(fixture.child).await.unwrap(),
        AgentRunPhase::Done
    );
    let observed = fixture.model.observed().await;
    assert_eq!(
        observed.len(),
        before + 1,
        "an answer accepted during finalization must reach a new model turn"
    );
    assert_eq!(
        serde_json::to_string(observed.last().unwrap())
            .unwrap()
            .matches("[user-answer id=")
            .count(),
        1
    );
    fixture.runtime.cancel(fixture.parent).unwrap();
    fixture.runtime.wait(fixture.parent).await.unwrap();
}

#[tokio::test]
async fn caller_interruption_releases_finalization_wait_and_preserves_answer() {
    use futures_util::FutureExt;

    for cancel in [false, true] {
        let gate = Arc::new(FinalizationGate::default());
        let fixture = fixture_with_hook(false, true, Some(gate.clone())).await;
        gate.entered.notified().await;
        let question = fixture
            .runtime
            .user_answers(fixture.child)
            .unwrap()
            .remove(0);
        let answering =
            fixture
                .runtime
                .answer_subagent_question(fixture.parent, &question.id, "Use A");
        tokio::pin!(answering);
        assert!(answering.as_mut().now_or_never().is_none());
        if cancel {
            fixture.runtime.cancel(fixture.parent).unwrap();
        } else {
            fixture
                .runtime
                .stop(fixture.parent, runtime::StopScope::SelfOnly)
                .unwrap();
        }
        let outcome = answering.as_mut().now_or_never();
        assert!(
            outcome.is_some(),
            "caller interruption must release the wait before the child finishes cleanup"
        );
        assert!(outcome.unwrap().is_err());
        assert_eq!(
            fixture
                .runtime
                .user_question(&question.id)
                .unwrap()
                .unwrap()
                .answer
                .as_deref(),
            Some("Use A")
        );
        gate.release.notify_one();
        fixture.runtime.wait(fixture.parent).await.unwrap();
        fixture.runtime.wait(fixture.child).await.unwrap();
        if !cancel {
            let registry_path = fixture._dir.path().join("owners.db");
            let owner = runtime::ownership::Registry::open(&registry_path)
                .unwrap()
                .attach("thread")
                .unwrap();
            fixture
                .runtime
                .resume_chat(
                    fixture.parent,
                    RunConfig {
                        ownership: Some(runtime::ownership::OwnerPermit {
                            registry_path,
                            thread_id: owner.thread_id,
                            lease: owner.lease,
                            run_id: None,
                        }),
                        ..Default::default()
                    },
                )
                .unwrap();
            fixture
                .runtime
                .answer_subagent_question(fixture.parent, &question.id, "Use A")
                .await
                .unwrap();
            assert_eq!(
                fixture.runtime.wait(fixture.child).await.unwrap(),
                AgentRunPhase::Done
            );
            assert_eq!(
                fixture
                    .model
                    .observed()
                    .await
                    .iter()
                    .map(|history| serde_json::to_string(history)
                        .unwrap()
                        .matches("[user-answer id=")
                        .count())
                    .sum::<usize>(),
                1
            );
            fixture.runtime.cancel(fixture.parent).unwrap();
            fixture.runtime.wait(fixture.parent).await.unwrap();
        }
    }
}

#[tokio::test]
async fn interruption_during_finalization_prevents_answer_continuation() {
    for cancel in [false, true] {
        let gate = Arc::new(FinalizationGate::default());
        let fixture = fixture_with_hook(false, true, Some(gate.clone())).await;
        gate.entered.notified().await;
        let question = fixture
            .runtime
            .user_answers(fixture.child)
            .unwrap()
            .remove(0);
        let before = fixture.model.observed().await.len();
        let mut events = fixture.bus.subscribe();
        let runtime = fixture.runtime.clone();
        let parent = fixture.parent;
        let id = question.id.clone();
        let answering =
            tokio::spawn(
                async move { runtime.answer_subagent_question(parent, &id, "Use A").await },
            );
        loop {
            if let EventKind::Tool(event_bus::ToolEvent::UserQuestionUpdated { question: updated }) =
                events.recv().await.unwrap().kind
                && updated.id == question.id
                && updated.answer.is_some()
            {
                break;
            }
        }
        if cancel {
            fixture.runtime.cancel(fixture.child).unwrap();
        } else {
            fixture
                .runtime
                .stop(fixture.child, runtime::StopScope::SelfOnly)
                .unwrap();
        }
        gate.release.notify_one();
        assert!(
            answering
                .await
                .unwrap()
                .unwrap_err()
                .contains("requires explicit recovery")
        );
        fixture.runtime.wait(fixture.child).await.unwrap();
        assert_eq!(fixture.model.observed().await.len(), before);
        fixture.runtime.cancel(fixture.parent).unwrap();
        fixture.runtime.wait(fixture.parent).await.unwrap();
    }
}

#[tokio::test]
async fn delayed_parent_answer_resumes_done_child_once_with_saved_history() {
    for owned in [false, true] {
        let Fixture {
            runtime,
            model,
            parent,
            child,
            storage: _storage,
            _dir,
            ..
        } = fixture(false, owned).await;
        assert_eq!(runtime.wait(child).await.unwrap(), AgentRunPhase::Done);
        let question = runtime.user_answers(child).unwrap().remove(0);
        let before = model.observed().await;
        if owned {
            assert!(matches!(
                runtime.send_agent_message(
                    parent,
                    child,
                    event_bus::AgentMessageKind::Send,
                    "ordinary delivery",
                    None
                ),
                Err(runtime::RuntimeError::RunRestoreFailed {
                    reason: runtime::RunRestoreFailure::UnsupportedConfig(_),
                    ..
                })
            ));
        }

        runtime
            .answer_subagent_question(parent, &question.id, "Use A")
            .await
            .unwrap();
        runtime
            .answer_subagent_question(parent, &question.id, "Use A")
            .await
            .unwrap();
        assert_eq!(runtime.wait(child).await.unwrap(), AgentRunPhase::Done);

        let observed = model.observed().await;
        assert_eq!(
            observed.len(),
            before.len() + 1,
            "the answer starts one new turn"
        );
        let last = observed.last().unwrap();
        assert!(last.starts_with(before.last().unwrap()));
        let history = serde_json::to_string(last).unwrap();
        assert!(history.contains("Independent work completed"));
        assert!(history.contains("Use A"));
        assert_eq!(history.matches("[user-answer id=").count(), 1);

        runtime
            .answer_subagent_question(parent, &question.id, "Use A")
            .await
            .unwrap();
        assert_eq!(runtime.wait(child).await.unwrap(), AgentRunPhase::Done);
        assert_eq!(model.observed().await.len(), observed.len());
        runtime.cancel(parent).unwrap();
        runtime.wait(parent).await.unwrap();
    }
}

#[tokio::test]
async fn live_waiting_child_reads_parent_answer_once() {
    let fixture = fixture(true, true).await;
    let mut events = fixture.bus.subscribe();
    while fixture.runtime.inspect_agent(fixture.child).unwrap().phase != AgentRunPhase::Waiting {
        if let EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged { run_id, to, .. }) =
            events.recv().await.unwrap().kind
            && run_id == fixture.child.to_string()
            && matches!(
                to,
                AgentRunPhase::Done | AgentRunPhase::Stopped | AgentRunPhase::Error
            )
        {
            panic!("child ended before waiting: {to:?}");
        }
    }
    let question = fixture
        .runtime
        .user_answers(fixture.child)
        .unwrap()
        .remove(0);
    fixture
        .runtime
        .answer_subagent_question(fixture.parent, &question.id, "Use A")
        .await
        .unwrap();
    assert_eq!(
        fixture.runtime.wait(fixture.child).await.unwrap(),
        AgentRunPhase::Done
    );
    let observed = fixture.model.observed().await;
    let last = serde_json::to_string(observed.last().unwrap()).unwrap();
    assert_eq!(last.matches("[user-answer id=").count(), 1);
    assert!(last.contains("Use A"));
    fixture.runtime.cancel(fixture.parent).unwrap();
    fixture.runtime.wait(fixture.parent).await.unwrap();
}

#[tokio::test]
async fn failed_child_restore_reports_error_and_identical_answer_can_retry() {
    let fixture = fixture(false, false).await;
    assert_eq!(
        fixture.runtime.wait(fixture.child).await.unwrap(),
        AgentRunPhase::Done
    );
    let question = fixture
        .runtime
        .user_answers(fixture.child)
        .unwrap()
        .remove(0);
    let before = fixture.model.observed().await.len();
    let connection = rusqlite::Connection::open(&fixture.config.db_path).unwrap();
    connection.execute_batch("CREATE TRIGGER reject_consume BEFORE UPDATE ON run_contexts WHEN NEW.restorable = 0 BEGIN SELECT RAISE(ABORT, 'injected consume failure'); END;").unwrap();
    let error = fixture
        .runtime
        .answer_subagent_question(fixture.parent, &question.id, "Use A")
        .await
        .unwrap_err();
    assert!(error.contains("injected consume failure"), "{error}");
    assert_eq!(
        fixture
            .runtime
            .user_question(&question.id)
            .unwrap()
            .unwrap()
            .answer
            .as_deref(),
        Some("Use A")
    );
    assert_eq!(fixture.model.observed().await.len(), before);
    connection
        .execute_batch("DROP TRIGGER reject_consume")
        .unwrap();
    fixture
        .runtime
        .answer_subagent_question(fixture.parent, &question.id, "Use A")
        .await
        .unwrap();
    assert_eq!(
        fixture.runtime.wait(fixture.child).await.unwrap(),
        AgentRunPhase::Done
    );
    assert_eq!(fixture.model.observed().await.len(), before + 1);
    fixture.runtime.cancel(fixture.parent).unwrap();
    fixture.runtime.wait(fixture.parent).await.unwrap();
}

#[tokio::test]
async fn stopped_cancelled_and_failed_children_are_not_restarted_by_answers() {
    for terminal in ["stopped", "cancelled", "failed"] {
        let fixture = fixture(true, false).await;
        let mut events = fixture.bus.subscribe();
        while fixture.runtime.inspect_agent(fixture.child).unwrap().phase != AgentRunPhase::Waiting
        {
            events.recv().await.unwrap();
        }
        let question = fixture
            .runtime
            .user_answers(fixture.child)
            .unwrap()
            .remove(0);
        match terminal {
            "stopped" => fixture
                .runtime
                .stop(fixture.child, runtime::StopScope::SelfOnly)
                .unwrap(),
            "cancelled" => fixture.runtime.cancel(fixture.child).unwrap(),
            _ => {
                fixture
                    .model
                    .add_keyed(
                        "CHILD",
                        [Err(runtime::RuntimeError::Model {
                            reason: "injected failure".into(),
                        })],
                    )
                    .await;
                fixture
                    .runtime
                    .send_message(fixture.child, "continue independently".into())
                    .unwrap();
            }
        }
        let phase = fixture.runtime.wait(fixture.child).await.unwrap();
        assert_eq!(
            phase,
            if terminal == "stopped" {
                AgentRunPhase::Stopped
            } else {
                AgentRunPhase::Error
            }
        );
        let before = fixture.model.observed().await.len();
        let error = fixture
            .runtime
            .answer_subagent_question(fixture.parent, &question.id, "Use A")
            .await
            .unwrap_err();
        assert!(error.contains("requires explicit recovery"), "{error}");
        assert_eq!(
            fixture.runtime.inspect_agent(fixture.child).unwrap().phase,
            phase
        );
        assert_eq!(fixture.model.observed().await.len(), before);
        fixture.runtime.cancel(fixture.parent).unwrap();
        fixture.runtime.wait(fixture.parent).await.unwrap();
    }
}

#[tokio::test]
async fn child_continuation_uses_current_parent_owner_and_fences_stale_parent() {
    use runtime::ownership::{OwnerPermit, Registry};
    let fixture = fixture(false, true).await;
    assert_eq!(
        fixture.runtime.wait(fixture.child).await.unwrap(),
        AgentRunPhase::Done
    );
    let question = fixture
        .runtime
        .user_answers(fixture.child)
        .unwrap()
        .remove(0);
    fixture
        .runtime
        .stop(fixture.parent, runtime::StopScope::SelfOnly)
        .unwrap();
    assert_eq!(
        fixture.runtime.wait(fixture.parent).await.unwrap(),
        AgentRunPhase::Stopped
    );
    let registry_path = fixture._dir.path().join("owners.db");
    let mut registry = Registry::open(&registry_path).unwrap();
    let old_lease = registry.attach("thread").unwrap().lease;
    registry
        .update("thread", |owner| {
            owner.quiesce(&old_lease)?;
            owner.release(&old_lease)?;
            owner.claim(&old_lease, "second", 0, 0)
        })
        .unwrap();
    let error = fixture
        .runtime
        .answer_subagent_question(fixture.parent, &question.id, "Use A")
        .await
        .unwrap_err();
    assert!(error.contains("ownership"), "{error}");
    assert!(
        fixture
            .runtime
            .user_question(&question.id)
            .unwrap()
            .unwrap()
            .answer
            .is_none()
    );
    fixture
        .runtime
        .resume_chat(
            fixture.parent,
            RunConfig {
                ownership: Some(OwnerPermit {
                    registry_path,
                    thread_id: "thread".into(),
                    lease: registry.attach("thread").unwrap().lease,
                    run_id: None,
                }),
                ..Default::default()
            },
        )
        .unwrap();
    fixture
        .runtime
        .answer_subagent_question(fixture.parent, &question.id, "Use A")
        .await
        .unwrap();
    assert_eq!(
        fixture.runtime.wait(fixture.child).await.unwrap(),
        AgentRunPhase::Done
    );
    let owner = registry.attach("thread").unwrap();
    assert_eq!(owner.lease.generation, old_lease.generation + 1);
    assert_eq!(owner.lease.owner_id, "second");
    assert!(!owner.active_runs.contains(&fixture.child.to_string()));
    let observed = fixture.model.observed().await;
    assert!(
        observed
            .iter()
            .any(|history| serde_json::to_string(history).unwrap().contains("Use A"))
    );
    fixture.runtime.cancel(fixture.parent).unwrap();
    fixture.runtime.wait(fixture.parent).await.unwrap();
}
