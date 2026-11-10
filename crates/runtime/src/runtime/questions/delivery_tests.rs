use super::*;
use crate::{RunConfig, RunStore};
use event_bus::{AgentRunPhase, EventBus, EventKind, LifecycleEvent};
use tokio::sync::Notify;

#[derive(Default)]
struct Model {
    first: Notify,
    finish: Notify,
    resumed: Notify,
    child_started: Notify,
    question_seen: Notify,
    middle_seen: Notify,
    middle_finish: Notify,
    middle_calls: std::sync::atomic::AtomicU32,
    middle_run: std::sync::Mutex<Option<RunId>>,
    child_inputs: std::sync::Mutex<Vec<Vec<providers::Message>>>,
    calls: std::sync::atomic::AtomicU32,
    finish_children: std::sync::atomic::AtomicBool,
    idle_each_turn: std::sync::atomic::AtomicBool,
}
#[async_trait::async_trait]
impl crate::AgentModel for Model {
    fn selected_model(&self, _: crate::Role, _: Option<&str>) -> String {
        "questions".into()
    }
    async fn complete(
        &self,
        invocation: &crate::AgentInvocationContext,
        role: crate::Role,
        messages: &[providers::Message],
        _: &[providers::ToolSpec],
    ) -> Result<providers::ChatResponse, crate::RuntimeError> {
        if role != crate::Role::Orchestrator {
            if self
                .middle_run
                .lock()
                .unwrap()
                .is_some_and(|run| run.to_string() == invocation.run_id)
            {
                self.middle_seen.notify_one();
                if self.middle_calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    self.middle_finish.notified().await;
                }
                return Ok(finished());
            }
            self.child_inputs.lock().unwrap().push(messages.to_vec());
            self.child_started.notify_one();
            if self.finish_children.load(Ordering::Acquire) {
                return Ok(finished());
            }
            return std::future::pending().await;
        }
        if messages.iter().flat_map(|m| &m.content).any(|block| matches!(block, providers::ContentBlock::Text { text } if text.contains("[subagent-question"))) {
            self.question_seen.notify_one();
        }
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            self.first.notify_one();
            self.finish.notified().await;
            Ok(finished())
        } else {
            self.resumed.notify_one();
            if self.idle_each_turn.load(Ordering::Acquire) {
                return Ok(finished());
            }
            std::future::pending().await
        }
    }
}
fn finished() -> providers::ChatResponse {
    providers::ChatResponse {
        message: providers::Message {
            role: providers::Role::Assistant,
            content: vec![providers::ContentBlock::Text {
                text: "finished".into(),
            }],
        },
        finish_reason: providers::FinishReason::Stop,
        usage: Default::default(),
        usage_reported: true,
    }
}
struct Fixture {
    runtime: AgentRuntime,
    model: Arc<Model>,
    _storage: storage::Storage,
    _dir: tempfile::TempDir,
}
fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let config = storage::StorageConfig {
        db_path: dir.path().join("questions.db"),
        ..Default::default()
    };
    let storage = storage::Storage::open(config.clone()).unwrap();
    let bus = Arc::new(EventBus::new(1024));
    let model = Arc::new(Model::default());
    let runtime = AgentRuntime::new(
        bus.clone(),
        Arc::new(tools::ToolExecutor::new(bus)),
        model.clone(),
    )
    .with_run_store(RunStore::open(&config, storage.handle()).unwrap());
    Fixture {
        runtime,
        model,
        _storage: storage,
        _dir: dir,
    }
}
fn restarted_runtime(f: &Fixture, model: Arc<Model>) -> AgentRuntime {
    let config = storage::StorageConfig {
        db_path: f._dir.path().join("questions.db"),
        ..Default::default()
    };
    let bus = Arc::new(EventBus::new(1024));
    AgentRuntime::new(bus.clone(), Arc::new(tools::ToolExecutor::new(bus)), model)
        .with_run_store(RunStore::open(&config, f._storage.handle()).unwrap())
}
async fn pair(f: &Fixture, interactive: bool) -> (RunId, RunId) {
    let root = f.runtime.delegate_background(
        crate::Role::Orchestrator,
        "parent".into(),
        RunConfig {
            interactive,
            keep_alive: interactive,
            ..Default::default()
        },
    );
    f.model.first.notified().await;
    let child = f
        .runtime
        .delegate_background_as_child(root, crate::Role::Worker, "child", RunConfig::default())
        .unwrap();
    f.model.child_started.notified().await;
    (root, child)
}
fn ask(f: &Fixture, child: RunId, blocking: bool) -> UserQuestion {
    f.runtime
        .request_user_question(child, "A clarification".into(), vec![], blocking)
        .unwrap()
}
async fn root_wake(rx: &mut event_bus::EventReceiver, root: RunId) {
    loop {
        if matches!(rx.recv().await.unwrap().kind, EventKind::Lifecycle(LifecycleEvent::RootWakeRequested { run_id }) if run_id == root.to_string())
        {
            return;
        }
    }
}

#[tokio::test]
async fn saved_question_notifies_running_parent_once_without_reply_authority() {
    let f = fixture();
    let (root, child) = pair(&f, false).await;
    let question = ask(&f, child, false);
    let messages = f.runtime.take_inbox(root).unwrap();
    assert_eq!(
        messages.len(),
        1,
        "saving a normal delegated question must notify its direct parent"
    );
    assert!(messages[0].content.contains(&question.id));
    assert_eq!(messages[0].kind, AgentMessageKind::Steering);
    assert!(messages[0].content.contains("answer_subagent_question"));
    assert!(
        f.runtime
            .send_agent_message(
                root,
                child,
                AgentMessageKind::Reply,
                "not an answer",
                Some(messages[0].message_id.clone())
            )
            .is_err()
    );
    f.runtime.queue_subagent_question(&question);
    assert!(f.runtime.take_inbox(root).unwrap().is_empty());
    assert!(f.runtime.question_reported(&question.id));
    assert!(
        f.runtime
            .user_question(&question.id)
            .unwrap()
            .unwrap()
            .answer
            .is_none()
    );
    f.runtime.cancel_subtree(root).unwrap();
}

#[tokio::test]
async fn idle_parent_is_woken_in_place_and_spends_only_its_child_guard() {
    let f = fixture();
    let (root, child) = pair(&f, true).await;
    let mut rx = f.runtime.shared.bus.subscribe();
    f.model.finish.notify_one();
    loop {
        if matches!(rx.recv().await.unwrap().kind, EventKind::Lifecycle(LifecycleEvent::TurnCompleted { run_id, .. }) if run_id == root.to_string())
        {
            break;
        }
    }
    let question = ask(&f, child, false);
    f.model.resumed.notified().await;
    assert!(f.runtime.question_reported(&question.id));
    assert_eq!(f.runtime.auto_wake_total(), 0);
    assert_eq!(
        f.runtime.shared.question_delivery.lock().unwrap().wakes[&(root, child)],
        1
    );
    assert_eq!(f.model.calls.load(Ordering::SeqCst), 2);
    f.runtime.cancel_subtree(root).unwrap();
}

#[tokio::test]
async fn terminal_root_wakes_for_blocking_only_and_rechecks_answer_before_host_delivery() {
    let f = fixture();
    let (root, child) = pair(&f, false).await;
    f.model.finish.notify_one();
    f.runtime.wait(root).await.unwrap();
    let mut rx = f.runtime.shared.bus.subscribe();
    let nonblocking = ask(&f, child, false);
    // Drive the actual wake future to completion: absence is not a timing assertion.
    f.runtime.wake(root).await;
    assert_eq!(f.runtime.auto_wake_total(), 0);
    assert!(
        !f.runtime
            .prepare_held_wake(root, &RunConfig::default())
            .unwrap()
    );
    let blocking = ask(&f, child, true);
    root_wake(&mut rx, root).await;
    assert!(
        f.runtime
            .prepare_held_wake(root, &RunConfig::default())
            .unwrap()
    );
    f.runtime
        .answer_user_question(&blocking.id, "host answered before wake")
        .unwrap();
    assert!(
        !f.runtime
            .prepare_held_wake(root, &RunConfig::default())
            .unwrap()
    );
    assert_eq!(f.model.calls.load(Ordering::SeqCst), 1);
    assert!(!f.runtime.question_reported(&nonblocking.id));
    assert!(f.runtime.question_reported(&blocking.id));
    f.runtime.cancel_subtree(root).unwrap();
}

#[tokio::test]
async fn terminal_root_restores_and_delivers_blocking_with_nonblocking_piggyback() {
    let f = fixture();
    let (root, child) = pair(&f, false).await;
    f.model.finish.notify_one();
    f.runtime.wait(root).await.unwrap();
    let mut rx = f.runtime.shared.bus.subscribe();
    let a = ask(&f, child, false);
    let b = ask(&f, child, true);
    root_wake(&mut rx, root).await;
    assert_eq!(f.runtime.auto_wake_total(), 0);
    assert_eq!(
        f.runtime.wake_chat(root, RunConfig::default()).unwrap(),
        root
    );
    f.model.resumed.notified().await;
    assert!(f.runtime.question_reported(&a.id));
    assert!(f.runtime.question_reported(&b.id));
    assert_eq!(
        f.runtime.shared.question_delivery.lock().unwrap().wakes[&(root, child)],
        1
    );
    f.runtime.cancel_subtree(root).unwrap();
}

#[tokio::test]
async fn stopped_parent_holds_question_and_host_answer_consumes_it_without_wake() {
    let f = fixture();
    let (root, child) = pair(&f, false).await;
    // A question racing Stop is fenced by this operator latch, not elapsed time.
    f.runtime.shared.operator_stops.lock().unwrap().insert(root);
    let q = ask(&f, child, true);
    f.runtime.wake(root).await;
    assert!(f.runtime.take_inbox(root).unwrap().is_empty());
    assert_eq!(f.runtime.held_message_count(), 1);
    assert!(
        !f.runtime
            .prepare_held_wake(root, &RunConfig::default())
            .unwrap()
    );
    f.runtime.answer_user_question(&q.id, "resolved").unwrap();
    f.runtime
        .shared
        .operator_stops
        .lock()
        .unwrap()
        .remove(&root);
    f.runtime.wake(root).await;
    assert!(
        !f.runtime
            .prepare_held_wake(root, &RunConfig::default())
            .unwrap()
    );
    assert_eq!(f.runtime.auto_wake_total(), 0);
    f.runtime.cancel_subtree(root).unwrap();
}

#[tokio::test]
async fn full_mailbox_keeps_question_until_capacity_returns() {
    let f = fixture();
    let (root, child) = pair(&f, false).await;
    let mailbox = f.runtime.run_mailbox(root).unwrap();
    for i in 0..crate::mailbox::MAILBOX_CAPACITY {
        mailbox
            .try_push(AgentMessage {
                message_id: format!("full-{i}"),
                sender_run_id: child.to_string(),
                recipient_run_id: root.to_string(),
                kind: AgentMessageKind::Send,
                content: "full".into(),
                reply_to: None,
            })
            .unwrap();
    }
    let q = ask(&f, child, true);
    assert_eq!(f.runtime.held_message_count(), 1);
    mailbox.drain_all();
    f.runtime.refill_question_mailbox(root);
    assert!(
        f.runtime
            .take_inbox(root)
            .unwrap()
            .iter()
            .any(|m| m.content.contains(&q.id))
    );
    assert_eq!(f.runtime.held_message_count(), 0);
    f.runtime.cancel_subtree(root).unwrap();
}

#[tokio::test]
async fn separate_child_guards_bound_twentieth_and_reset_only_on_user_signal() {
    let f = fixture();
    let (root, child) = pair(&f, false).await;
    for i in 0..QUESTION_WAKE_LIMIT {
        let n = Arc::new(QuestionNotice::new(format!("guard-{i}"), root, child, true));
        assert!(f.runtime.admit_question_batch(root, root, &[n], true));
    }
    let exhausted = Arc::new(QuestionNotice::new("guard-21".into(), root, child, true));
    assert!(
        !f.runtime
            .admit_question_batch(root, root, std::slice::from_ref(&exhausted), true)
    );
    assert!(exhausted.paused.load(Ordering::Acquire));
    // More than twenty distinct children are normal, not a shared wake storm.
    for i in 0..=QUESTION_WAKE_LIMIT {
        let other = f.runtime.reserve_run_id();
        assert!(f.runtime.admit_question_batch(
            root,
            root,
            &[Arc::new(QuestionNotice::new(
                format!("other-{i}"),
                root,
                other,
                true
            ))],
            true
        ));
    }
    f.runtime.observe_questions(["guard-21".into()]);
    assert_eq!(
        f.runtime.shared.question_delivery.lock().unwrap().wakes[&(root, child)],
        20
    );
    f.runtime.reset_auto_wakes(root);
    assert!(f.runtime.admit_question_batch(
        root,
        root,
        &[Arc::new(QuestionNotice::new(
            "reset".into(),
            root,
            child,
            true
        ))],
        true
    ));
    assert_eq!(f.runtime.auto_wake_total(), 0);
    f.runtime.cancel_subtree(root).unwrap();
}

#[tokio::test]
async fn mixed_held_batch_uses_auto_budget_or_falls_back_to_question_budget() {
    for initial_auto in [0, 20, 21] {
        let auto_paused = initial_auto >= 20;
        let f = fixture();
        let (root, child) = pair(&f, false).await;
        f.model.finish.notify_one();
        f.runtime.wait(root).await.unwrap();
        if auto_paused {
            f.runtime
                .shared
                .auto_wakes
                .lock()
                .unwrap()
                .insert(root, initial_auto);
        }
        f.runtime.hold_message(
            root,
            AgentMessage {
                message_id: "piggyback".into(),
                sender_run_id: child.to_string(),
                recipient_run_id: root.to_string(),
                kind: AgentMessageKind::Send,
                content: "held report".into(),
                reply_to: None,
            },
        );
        let mut rx = f.runtime.shared.bus.subscribe();
        let q = ask(&f, child, true);
        root_wake(&mut rx, root).await;
        assert_eq!(
            f.runtime.auto_wake_total(),
            if auto_paused { initial_auto } else { 1 }
        );
        assert_eq!(
            f.runtime
                .shared
                .question_delivery
                .lock()
                .unwrap()
                .wakes
                .get(&(root, child))
                .copied()
                .unwrap_or(0),
            u32::from(auto_paused)
        );
        f.runtime.wake_chat(root, RunConfig::default()).unwrap();
        f.model.resumed.notified().await;
        assert!(f.runtime.question_reported(&q.id));
        assert_eq!(f.runtime.held_message_count(), 0);
        f.runtime.cancel_subtree(root).unwrap();
    }
}

#[tokio::test]
async fn explicit_resume_after_restart_recovers_question_then_restores_child_under_parent() {
    let f = fixture();
    let (root, child) = pair(&f, false).await;
    let question = ask(&f, child, true);
    f.runtime.stop(root, crate::StopScope::Subtree).unwrap();
    f.runtime.wait(root).await.unwrap();
    f.runtime.wait(child).await.unwrap();
    let config = storage::StorageConfig {
        db_path: f._dir.path().join("questions.db"),
        ..Default::default()
    };
    let bus = Arc::new(EventBus::new(1024));
    let model = Arc::new(Model::default());
    let runtime = AgentRuntime::new(
        bus.clone(),
        Arc::new(tools::ToolExecutor::new(bus)),
        model.clone(),
    )
    .with_run_store(RunStore::open(&config, f._storage.handle()).unwrap());
    assert!(
        runtime.list_agents().is_empty(),
        "no startup root auto-recovery is added"
    );
    runtime.resume_chat(root, RunConfig::default()).unwrap();
    model.first.notified().await;
    // Recovery may append before the first request or at the next boundary.
    // Await the request that actually contains the notice, not a turn count.
    model.finish.notify_one();
    model.question_seen.notified().await;
    assert!(runtime.question_reported(&question.id));
    assert_eq!(
        runtime.subagent_questions(root, child).unwrap()[0].id,
        question.id
    );
    assert!(
        runtime.entry(child).is_err(),
        "child is restored only after its parent resumes and answers"
    );
    let answered = runtime
        .answer_subagent_question(root, &question.id, "Scope from the resumed parent")
        .await
        .unwrap();
    model.child_started.notified().await;
    assert_eq!(
        answered.answer_origin,
        Some(event_bus::AnswerOrigin::ParentAgent {
            answered_by_run_id: root.to_string()
        })
    );
    assert_eq!(runtime.entry(child).unwrap().parent, Some(root));
    let inputs = serde_json::to_string(&*model.child_inputs.lock().unwrap()).unwrap();
    assert!(inputs.contains("Scope from the resumed parent"));
    runtime.cancel_subtree(root).unwrap();
    runtime.wait(child).await.unwrap();
    runtime.wait(root).await.unwrap();
}

#[tokio::test]
async fn stop_after_host_preparation_cannot_be_undone_by_delayed_wake() {
    let f = fixture();
    let (root, child) = pair(&f, false).await;
    f.model.finish.notify_one();
    f.runtime.wait(root).await.unwrap();
    let mut rx = f.runtime.shared.bus.subscribe();
    let q = ask(&f, child, true);
    root_wake(&mut rx, root).await;
    assert!(
        f.runtime
            .prepare_held_wake(root, &RunConfig::default())
            .unwrap()
    );
    f.runtime.stop(root, crate::StopScope::SelfOnly).unwrap();
    assert!(f.runtime.wake_chat(root, RunConfig::default()).is_err());
    assert_eq!(f.model.calls.load(Ordering::SeqCst), 1);
    assert!(!f.runtime.question_reported(&q.id));
    assert_eq!(f.runtime.held_message_count(), 1);
    f.runtime.cancel_subtree(root).unwrap();
}

#[tokio::test]
async fn saved_answer_consumes_notice_even_when_stopped_child_refuses_continuation() {
    let f = fixture();
    let (root, child) = pair(&f, false).await;
    let q = ask(&f, child, true);
    f.runtime.stop(child, crate::StopScope::SelfOnly).unwrap();
    f.runtime.wait(child).await.unwrap();
    assert!(
        f.runtime
            .answer_subagent_question(root, &q.id, "Saved scope")
            .await
            .is_err()
    );
    let saved = f.runtime.user_question(&q.id).unwrap().unwrap();
    assert_eq!(saved.answer.as_deref(), Some("Saved scope"));
    assert_eq!(
        saved.answer_origin,
        Some(event_bus::AnswerOrigin::ParentAgent {
            answered_by_run_id: root.to_string()
        })
    );
    assert!(f.runtime.question_reported(&q.id));
    f.runtime.queue_subagent_question(&q);
    assert!(
        f.runtime
            .take_inbox(root)
            .unwrap()
            .iter()
            .all(|m| !m.content.contains(&q.id))
    );
    assert_eq!(f.runtime.auto_wake_total(), 0);
    f.runtime.cancel_subtree(root).unwrap();
}

#[tokio::test]
async fn held_subagent_report_is_retried_when_its_ancestor_is_explicitly_resumed() {
    let f = fixture();
    let root = f.runtime.delegate_background(
        crate::Role::Orchestrator,
        "parent".into(),
        RunConfig::default(),
    );
    f.model.first.notified().await;
    let middle = f.runtime.reserve_child_run_id(root).unwrap();
    *f.model.middle_run.lock().unwrap() = Some(middle);
    f.runtime
        .spawn_reserved_child(
            root,
            middle,
            crate::Role::Worker,
            "middle",
            RunConfig::default(),
        )
        .unwrap();
    f.model.middle_seen.notified().await;
    let child = f
        .runtime
        .delegate_background_as_child(middle, crate::Role::Worker, "child", RunConfig::default())
        .unwrap();
    f.model.child_started.notified().await;
    // Admit the grandchild while its direct parent is still live.
    f.model.middle_finish.notify_one();
    f.runtime.wait(middle).await.unwrap();
    let output = f.runtime.run_output(root, middle).unwrap();
    f.runtime
        .observe_completion(output.completion_notice.as_ref().unwrap());
    f.model.finish.notify_one();
    f.runtime.wait(root).await.unwrap();
    // Model a report already held for renewed ancestor authority. The default
    // disk-restorable fixture would otherwise restore on send immediately.
    f.runtime.hold_queued_message(
        middle,
        QueuedMessage::explicit(event_bus::AgentMessage {
            message_id: "ancestor-held-report".into(),
            sender_run_id: child.to_string(),
            recipient_run_id: middle.to_string(),
            kind: AgentMessageKind::Send,
            content: "held report".into(),
            reply_to: None,
        }),
    );
    f.runtime.wake(middle).await;
    assert_eq!(f.runtime.held_message_count(), 1);
    assert_eq!(
        f.runtime.auto_wake_total(),
        0,
        "an unavailable ancestor must not spend a wake"
    );
    // Nested Orchestrators are forbidden by ADR 0044. A legal Worker subtree
    // exercises the same held-queue hook without bypassing that role policy.
    f.runtime.resume_chat(root, RunConfig::default()).unwrap();
    f.model.middle_seen.notified().await;
    assert_eq!(f.runtime.held_message_count(), 0);
    assert_eq!(f.runtime.auto_wake_total(), 1);
    f.runtime.cancel_subtree(root).unwrap();
    f.runtime.wait(child).await.unwrap();
    f.runtime.wait(root).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn question_accepted_after_final_flush_survives_mailbox_close() {
    let f = fixture();
    let (root, child) = pair(&f, false).await;
    let (entered, closing) = tokio::sync::oneshot::channel();
    let release = Arc::new(std::sync::Barrier::new(2));
    let gate = release.clone();
    f.runtime.on_execution(
        crate::runtime::cancellation::HookPoint::QuestionMailboxClosing,
        &f.runtime.execution_snapshot(root).unwrap(),
        move || {
            entered.send(()).unwrap();
            gate.wait();
        },
    );
    let mut events = f.runtime.shared.bus.subscribe();
    f.model.finish.notify_one();
    closing.await.unwrap();
    let question = ask(&f, child, true);
    assert_eq!(f.runtime.run_mailbox(root).unwrap().len(), 1);
    release.wait();
    assert_eq!(f.runtime.wait(root).await.unwrap(), AgentRunPhase::Done);
    assert_eq!(f.runtime.held_message_count(), 1);
    assert!(!f.runtime.question_reported(&question.id));
    root_wake(&mut events, root).await;
    f.runtime.wake_chat(root, RunConfig::default()).unwrap();
    f.model.question_seen.notified().await;
    assert!(f.runtime.question_reported(&question.id));
    assert_eq!(f.runtime.held_message_count(), 0);
    f.runtime.cancel_subtree(root).unwrap();
    f.runtime.wait(root).await.unwrap();
    f.runtime.wait(child).await.unwrap();
}

#[tokio::test]
async fn repeated_answer_after_restart_does_not_execute_an_already_answered_child() {
    let f = fixture();
    let (root, child) = pair(&f, false).await;
    let question = ask(&f, child, true);
    f.runtime.stop(root, crate::StopScope::Subtree).unwrap();
    f.runtime.wait(root).await.unwrap();
    f.runtime.wait(child).await.unwrap();

    let model = Arc::new(Model::default());
    model.finish_children.store(true, Ordering::Release);
    let runtime = restarted_runtime(&f, model.clone());
    runtime.resume_chat(root, RunConfig::default()).unwrap();
    model.first.notified().await;
    runtime
        .answer_subagent_question(root, &question.id, "Execute once")
        .await
        .unwrap();
    assert_eq!(runtime.wait(child).await.unwrap(), AgentRunPhase::Done);
    assert_eq!(model.child_inputs.lock().unwrap().len(), 1);
    runtime.stop(root, crate::StopScope::Subtree).unwrap();
    runtime.wait(root).await.unwrap();

    let next_model = Arc::new(Model::default());
    let next = restarted_runtime(&f, next_model.clone());
    next.resume_chat(root, RunConfig::default()).unwrap();
    next_model.first.notified().await;
    next.answer_subagent_question(root, &question.id, "Execute once")
        .await
        .unwrap();
    assert!(
        next.entry(child).is_err(),
        "a duplicate answer needs no child registration"
    );
    assert!(next_model.child_inputs.lock().unwrap().is_empty());
    assert_eq!(
        next.user_question(&question.id)
            .unwrap()
            .unwrap()
            .answer
            .as_deref(),
        Some("Execute once")
    );
    next.cancel_subtree(root).unwrap();
    next.wait(root).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parent_subtree_stop_fences_unregistered_answered_child_registration() {
    let f = fixture();
    let (root, child) = pair(&f, false).await;
    let question = ask(&f, child, true);
    f.runtime.stop(root, crate::StopScope::Subtree).unwrap();
    f.runtime.wait(root).await.unwrap();
    f.runtime.wait(child).await.unwrap();

    let model = Arc::new(Model::default());
    let runtime = restarted_runtime(&f, model.clone());
    runtime.resume_chat(root, RunConfig::default()).unwrap();
    model.first.notified().await;
    let (entered, prepared) = tokio::sync::oneshot::channel();
    let release = Arc::new(std::sync::Barrier::new(2));
    let gate = release.clone();
    runtime.on_execution(
        crate::runtime::cancellation::HookPoint::QuestionChildPrepared,
        &runtime.execution_snapshot(root).unwrap(),
        move || {
            entered.send(()).unwrap();
            gate.wait();
        },
    );
    let answering = runtime.clone();
    let id = question.id.clone();
    let answer = tokio::spawn(async move {
        answering
            .answer_subagent_question(root, &id, "Saved before Stop")
            .await
    });
    prepared.await.unwrap();
    assert!(runtime.entry(child).is_err());
    runtime.stop(root, crate::StopScope::Subtree).unwrap();
    release.wait();
    assert!(answer.await.unwrap().is_err());
    assert_eq!(runtime.wait(root).await.unwrap(), AgentRunPhase::Stopped);
    assert!(runtime.entry(child).is_err());
    assert!(model.child_inputs.lock().unwrap().is_empty());
    assert_eq!(
        runtime
            .user_question(&question.id)
            .unwrap()
            .unwrap()
            .answer
            .as_deref(),
        Some("Saved before Stop")
    );
}

async fn root_idle(events: &mut event_bus::EventReceiver, root: RunId) {
    loop {
        if matches!(events.recv().await.unwrap().kind, EventKind::Lifecycle(LifecycleEvent::TurnCompleted { run_id, .. }) if run_id == root.to_string())
        {
            return;
        }
    }
}

#[tokio::test]
async fn actual_idle_question_wakes_stop_at_twenty_until_real_user_input() {
    let f = fixture();
    f.model.idle_each_turn.store(true, Ordering::Release);
    let (root, child) = pair(&f, true).await;
    let mut events = f.runtime.shared.bus.subscribe();
    f.model.finish.notify_one();
    root_idle(&mut events, root).await;
    for expected in 1..=QUESTION_WAKE_LIMIT {
        let question = ask(&f, child, false);
        root_idle(&mut events, root).await;
        assert!(f.runtime.question_reported(&question.id));
        f.runtime
            .answer_subagent_question(root, &question.id, "Continue")
            .await
            .unwrap();
        assert_eq!(
            f.runtime.shared.question_delivery.lock().unwrap().wakes[&(root, child)],
            expected,
            "answers, new IDs and Running/Waiting transitions must not reset the guard"
        );
    }
    let held = ask(&f, child, false);
    loop {
        if matches!(events.recv().await.unwrap().kind, EventKind::Lifecycle(LifecycleEvent::AutoContinuationPaused { run_id, limit: QUESTION_WAKE_LIMIT }) if run_id == root.to_string())
        {
            break;
        }
    }
    assert_eq!(
        f.model.calls.load(Ordering::Acquire),
        QUESTION_WAKE_LIMIT + 1
    );
    assert!(!f.runtime.question_reported(&held.id));
    assert_eq!(f.runtime.held_message_count(), 1);
    assert_eq!(f.runtime.auto_wake_total(), 0);
    f.runtime
        .send_message(root, "Actual user continuation".into())
        .unwrap();
    root_idle(&mut events, root).await;
    assert!(f.runtime.question_reported(&held.id));
    assert!(
        f.runtime
            .shared
            .question_delivery
            .lock()
            .unwrap()
            .wakes
            .get(&(root, child))
            .copied()
            .unwrap_or(0)
            <= 1
    );
    f.runtime.cancel_subtree(root).unwrap();
    f.runtime.wait(root).await.unwrap();
    f.runtime.wait(child).await.unwrap();
}
