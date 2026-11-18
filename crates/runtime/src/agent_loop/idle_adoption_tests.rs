use super::delegate_cleanup_tests::started_fixture;
use super::*;

fn adopt(state: &mut LoopState) {
    let mut input = None;
    state
        .channels
        .user_inbox
        .enqueue(None, |seq| {
            input = Some(("accepted input".into(), true, None, seq));
            Ok::<_, ()>(())
        })
        .unwrap();
    assert!(state.take_user_input(input.unwrap()));
    assert_eq!(state.channels.user_inbox.status().pending, 0);
    assert!(state.channels.user_inbox.adopting());
}

async fn cleanup(runtime: crate::AgentRuntime, run: RunId) {
    runtime.cancel(run).unwrap();
    runtime.wait(run).await.unwrap();
}

#[tokio::test]
async fn adoption_survives_claim_and_is_released_after_running_is_published() {
    let (_cancel, cancel_rx) = watch::channel(RunInterrupt::None);
    let (runtime, mut state) = started_fixture(cancel_rx).await;
    let run = state.task.run_id;
    state.transition(AgentRunPhase::Running, None).unwrap();
    state
        .transition_with_turn(AgentRunPhase::Waiting, None, Some(Some("old turn".into())))
        .unwrap();
    let inbox = state.channels.user_inbox.clone();
    let phase = state.channels.phase_tx.subscribe();
    let (reached, held) = tokio::sync::oneshot::channel();
    let (resume, resumed) = std::sync::mpsc::channel();
    inbox.gate_next_adoption(reached, resumed);
    // A synchronous claim is deliberately blocked on another thread; the gate
    // owns no runtime, consumption or inbox-state lock.
    let consumer = std::thread::spawn(move || {
        adopt(&mut state);
        state
    });
    held.await.unwrap();
    assert_eq!(inbox.status().pending, 0);
    assert!(inbox.adopting());
    assert_eq!(*phase.borrow(), AgentRunPhase::Waiting);
    resume.send(()).unwrap();
    let mut state = consumer.join().unwrap();
    assert!(
        inbox.adopting(),
        "returning from claim is not phase publication"
    );
    state.transition(AgentRunPhase::Running, None).unwrap();
    assert_eq!(*phase.borrow(), AgentRunPhase::Running);
    assert!(!inbox.adopting());
    drop(state);
    cleanup(runtime, run).await;
}

#[tokio::test]
async fn input_adopted_while_running_releases_at_the_completed_turn_boundary() {
    let (_cancel, cancel_rx) = watch::channel(RunInterrupt::None);
    let (runtime, mut state) = started_fixture(cancel_rx).await;
    let run = state.task.run_id;
    state.transition(AgentRunPhase::Running, None).unwrap();
    adopt(&mut state);
    state
        .transition_with_turn(AgentRunPhase::Waiting, None, Some(None))
        .unwrap();
    assert_eq!(*state.channels.phase_tx.borrow(), AgentRunPhase::Waiting);
    assert!(!state.channels.user_inbox.adopting());
    drop(state);
    cleanup(runtime, run).await;
}

#[tokio::test]
async fn failed_phase_publication_keeps_adoption_until_loop_state_drops() {
    let (_cancel, cancel_rx) = watch::channel(RunInterrupt::None);
    let (runtime, mut state) = started_fixture(cancel_rx).await;
    let run = state.task.run_id;
    let inbox = state.channels.user_inbox.clone();
    adopt(&mut state);
    // Pending -> Waiting is invalid: no phase has replaced the old public state.
    assert!(state.transition(AgentRunPhase::Waiting, None).is_err());
    assert!(inbox.adopting());
    drop(state);
    assert!(!inbox.adopting());
    cleanup(runtime, run).await;
}

#[tokio::test]
async fn terminal_error_cancel_and_abandoned_loop_state_release_adoption() {
    for terminal in [
        Some(AgentRunPhase::Error),
        Some(AgentRunPhase::Stopped),
        None,
    ] {
        let (_cancel, cancel_rx) = watch::channel(RunInterrupt::None);
        let (runtime, mut state) = started_fixture(cancel_rx).await;
        let run = state.task.run_id;
        state.transition(AgentRunPhase::Running, None).unwrap();
        state.transition(AgentRunPhase::Waiting, None).unwrap();
        let inbox = state.channels.user_inbox.clone();
        adopt(&mut state);
        match terminal {
            Some(AgentRunPhase::Error) => state.finish_error("adoption failed".into()),
            Some(AgentRunPhase::Stopped) => state.finish_interrupted(InterruptKind::Cancel),
            None => {}
            _ => unreachable!(),
        }
        // Terminal publication is deferred until teardown. Do not drop the
        // bridge early while the old Waiting phase can still be observed.
        assert_eq!(*state.channels.phase_tx.borrow(), AgentRunPhase::Waiting);
        assert!(inbox.adopting());
        drop(state);
        assert!(!inbox.adopting());
        cleanup(runtime, run).await;
    }
}

// The public seam used by the core regression must hold a real interactive
// loop, not just a standalone inbox. Lifecycle events are completion signals.
pub(super) async fn wait_for_phase(
    events: &mut event_bus::EventReceiver,
    run: RunId,
    phase: AgentRunPhase,
) {
    loop {
        if let event_bus::EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
            run_id,
            to,
            ..
        }) = events.recv().await.unwrap().kind
            && run_id == run.to_string()
        {
            if to == phase {
                return;
            }
            assert!(
                !matches!(
                    to,
                    AgentRunPhase::Done | AgentRunPhase::Error | AgentRunPhase::Stopped
                ),
                "unexpected terminal phase: {to:?}"
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn public_gate_observes_real_adoption_and_cannot_stall_another_runtime() {
    use crate::escalation_review::support::{ScriptedModel, text_response};
    for cancel in [false, true] {
        let bus = Arc::new(EventBus::new(128));
        let mut events = bus.subscribe();
        let dir = tempfile::tempdir().unwrap();
        let config = storage::StorageConfig {
            db_path: dir.path().join("adoption.sqlite3"),
            ..Default::default()
        };
        let storage = storage::Storage::open(config.clone()).unwrap();
        let model = Arc::new(ScriptedModel::new([
            Ok(text_response("first", FinishReason::Stop)),
            Ok(text_response("second", FinishReason::Stop)),
        ]));
        let runtime =
            crate::AgentRuntime::new(bus.clone(), Arc::new(ToolExecutor::new(bus)), model.clone())
                .with_run_store(crate::RunStore::open(&config, storage.handle()).unwrap());
        let run = runtime
            .delegate_chat(
                "adoption",
                Role::Orchestrator,
                "first".into(),
                RunConfig {
                    interactive: true,
                    keep_alive: true,
                    ..RunConfig::default()
                },
            )
            .unwrap();
        wait_for_phase(&mut events, run, AgentRunPhase::Waiting).await;
        // Hold the resumed model: Waiting publication alone does not signal
        // completion of subsequent readiness polls or ignored wakes.
        let model_resume = Arc::new(tokio::sync::Notify::new());
        model.gate_key("first", model_resume.clone()).await;
        let (reached, held) = tokio::sync::oneshot::channel();
        let (resume, resumed) = std::sync::mpsc::channel();
        runtime
            .gate_next_user_input_adoption(run, reached, resumed)
            .unwrap();
        runtime.send_user_input(run, "next".into(), None).unwrap();
        held.await.unwrap();
        assert_eq!(runtime.follow_up_status(run).unwrap().pending, 0);
        assert!(runtime.is_adopting_user_input(run));
        assert!(
            runtime.is_adopting_user_input(run),
            "observation must not consume the guard"
        );

        let other_bus = Arc::new(EventBus::new(64));
        let other = crate::AgentRuntime::new(
            other_bus.clone(),
            Arc::new(ToolExecutor::new(other_bus)),
            Arc::new(ScriptedModel::new([Ok(text_response(
                "unrelated",
                FinishReason::Stop,
            ))])),
        );
        let other_run =
            other.delegate_background(Role::Orchestrator, "other".into(), RunConfig::default());
        assert_eq!(other.wait(other_run).await.unwrap(), AgentRunPhase::Done);
        assert!(!other.is_adopting_user_input(other_run));
        assert!(runtime.is_adopting_user_input(run));
        if cancel {
            runtime.cancel(run).unwrap();
        }
        resume.send(()).unwrap();
        if cancel {
            runtime.wait(run).await.unwrap();
        } else {
            model.wait_for_request(1).await;
        }
        assert!(!runtime.is_adopting_user_input(run));
        if !cancel {
            model_resume.notify_one();
            wait_for_phase(&mut events, run, AgentRunPhase::Waiting).await;
            cleanup(runtime, run).await;
        }
    }
}
