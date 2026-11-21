//! Every way a run enters Waiting records a typed cause on the same event, and the
//! event that leaves Waiting carries none.

use super::delegate_cleanup_tests::{fixture, store_fixture};
use super::*;
use crate::AgentRuntime;
use event_bus::{AgentMessage, AgentMessageKind, EventKind, WaitingMode, WaitingRelation};
use futures_util::poll;
use tokio::sync::mpsc;

type Transition = (AgentRunPhase, Option<String>, Option<WaitingCause>);

fn transitions(events: Vec<Event>, run: RunId) -> Vec<Transition> {
    let run = run.to_string();
    events
        .into_iter()
        .filter_map(|event| match event.kind {
            EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
                run_id,
                to,
                reason,
                waiting,
                ..
            }) if run_id == run => Some((to, reason, waiting)),
            _ => None,
        })
        .collect()
}

fn terminal_child(runtime: &AgentRuntime, parent: RunId) -> RunId {
    let child = runtime
        .delegate_background_as_child(parent, Role::Worker, "child", RunConfig::default())
        .unwrap();
    runtime.publish_terminal(
        child,
        LifecycleEvent::AgentRunStateChanged {
            run_id: child.to_string(),
            from: AgentRunPhase::Running,
            to: AgentRunPhase::Done,
            reason: None,
            waiting: None,
        },
    );
    child
}

fn wake_message(state: &LoopState) -> AgentMessage {
    AgentMessage {
        message_id: "wake".into(),
        sender_run_id: "run-99".into(),
        recipient_run_id: state.task.run_id.to_string(),
        kind: AgentMessageKind::Send,
        content: "input".into(),
        reply_to: None,
    }
}

/// Parks `state` at an input boundary, wakes it with a mailbox message and returns the
/// run's transitions.
async fn park_and_wake(
    state: &mut LoopState,
    completed_turn: bool,
    blocking: bool,
) -> Vec<Transition> {
    let (_inbox, inbox_rx) = mpsc::channel(1);
    state.channels.inbox_rx = inbox_rx.into();
    let mut events = state.shared.bus.subscribe();
    let mailbox = state.task.mailbox.clone();
    let message = wake_message(state);
    let phase = state.channels.phase_tx.subscribe();
    let run = state.task.run_id;
    {
        let waiting = async {
            if blocking {
                state.wait_for_input().await
            } else {
                state.wait_for_input_boundary(completed_turn).await
            }
        };
        tokio::pin!(waiting);
        assert!(poll!(&mut waiting).is_pending());
        assert_eq!(*phase.borrow(), AgentRunPhase::Waiting);
        mailbox.try_push(message).unwrap();
        assert!(waiting.await);
    }
    transitions(events.drain_pending_snapshot(), run)
}

#[tokio::test]
async fn a_finished_interactive_turn_records_turn_completed_and_resuming_clears_it() {
    let (_cancel, cancel_rx) = watch::channel(RunInterrupt::None);
    let (_runtime, mut state) = fixture(cancel_rx);
    state.transition(AgentRunPhase::Running, None).unwrap();

    let recorded = park_and_wake(&mut state, true, false).await;

    assert_eq!(
        recorded,
        [
            (
                AgentRunPhase::Waiting,
                None,
                Some(WaitingCause::TurnCompleted)
            ),
            (AgentRunPhase::Running, None, None),
        ]
    );
}

#[tokio::test]
async fn an_unanswered_nonblocking_question_makes_the_boundary_a_plain_input_boundary() {
    let dir = tempfile::tempdir().unwrap();
    let config = storage::StorageConfig {
        db_path: dir.path().join("nonblocking.db"),
        ..Default::default()
    };
    let storage = storage::Storage::open(config.clone()).unwrap();
    let store = crate::RunStore::open(&config, storage.handle()).unwrap();
    let (_cancel, cancel_rx) = watch::channel(RunInterrupt::None);
    let (_runtime, mut state) = store_fixture(cancel_rx, store);
    state.transition(AgentRunPhase::Running, None).unwrap();
    let asked = crate::meta::dispatch(
        &mut state,
        "ask_user",
        serde_json::json!({"title":"later","blocking":false}),
    )
    .await;
    assert!(!asked.result.is_error, "{:?}", asked.result.content);

    let recorded = park_and_wake(&mut state, true, false).await;

    assert_eq!(
        recorded,
        [
            (
                AgentRunPhase::Waiting,
                None,
                Some(WaitingCause::InputBoundary)
            ),
            (AgentRunPhase::Running, None, None),
        ]
    );
}

#[tokio::test]
async fn waiting_on_blocking_questions_records_its_own_cause() {
    let (_cancel, cancel_rx) = watch::channel(RunInterrupt::None);
    let (_runtime, mut state) = fixture(cancel_rx);
    state.transition(AgentRunPhase::Running, None).unwrap();

    let recorded = park_and_wake(&mut state, false, true).await;

    assert_eq!(
        recorded,
        [
            (
                AgentRunPhase::Waiting,
                None,
                Some(WaitingCause::BlockingQuestions)
            ),
            (AgentRunPhase::Running, None, None),
        ]
    );
}

#[tokio::test]
async fn the_wait_tool_records_its_call_mode_and_the_runs_it_saw_then_clears_on_return() {
    let (_cancel, cancel_rx) = watch::channel(RunInterrupt::None);
    let (runtime, mut state) = fixture(cancel_rx);
    state.transition(AgentRunPhase::Running, None).unwrap();
    let parent = state.task.run_id;
    let child = terminal_child(&runtime, parent);
    let mut events = state.shared.bus.subscribe();

    for (input, call, explicit, mode) in [
        (
            serde_json::json!({"run_id": child.to_string()}),
            "call-explicit",
            true,
            WaitingMode::Any,
        ),
        (
            serde_json::json!({"run_ids": [child.to_string()], "mode": "all"}),
            "call-all",
            true,
            WaitingMode::All,
        ),
    ] {
        state.meta_call_id = Some(call.into());
        let result = crate::meta::dispatch(&mut state, "wait", input).await;
        assert!(!result.result.is_error, "{:?}", result.result.content);

        assert_eq!(
            transitions(events.drain_pending_snapshot(), parent),
            [
                (
                    AgentRunPhase::Waiting,
                    None,
                    Some(WaitingCause::WaitTool {
                        call_id: Some(call.into()),
                        explicit,
                        mode,
                        runs: vec![event_bus::WaitingRun {
                            run_id: child.to_string(),
                            relation: WaitingRelation::Child,
                        }],
                    })
                ),
                (AgentRunPhase::Running, None, None),
            ]
        );
    }
}

#[tokio::test]
async fn an_automatic_wait_records_that_it_named_no_run_and_the_children_it_found() {
    let (_cancel, cancel_rx) = watch::channel(RunInterrupt::None);
    let (runtime, mut state) = fixture(cancel_rx);
    state.transition(AgentRunPhase::Running, None).unwrap();
    let parent = state.task.run_id;
    let child = terminal_child(&runtime, parent);
    let mut events = state.shared.bus.subscribe();
    state.meta_call_id = Some("call-auto".into());

    let result = crate::meta::dispatch(&mut state, "wait", serde_json::json!({})).await;

    assert!(!result.result.is_error, "{:?}", result.result.content);
    assert_eq!(
        transitions(events.drain_pending_snapshot(), parent)[0],
        (
            AgentRunPhase::Waiting,
            None,
            Some(WaitingCause::WaitTool {
                call_id: Some("call-auto".into()),
                explicit: false,
                mode: WaitingMode::Any,
                runs: vec![event_bus::WaitingRun {
                    run_id: child.to_string(),
                    relation: WaitingRelation::Child,
                }],
            })
        )
    );
}

#[tokio::test]
async fn delegate_waits_record_each_call_with_its_child() {
    let (_cancel, cancel_rx) = watch::channel(RunInterrupt::None);
    let (runtime, mut state) = fixture(cancel_rx);
    state.transition(AgentRunPhase::Running, None).unwrap();
    let parent = state.task.run_id;
    let first = terminal_child(&runtime, parent);
    let second = terminal_child(&runtime, parent);
    let mut events = state.shared.bus.subscribe();

    // A wave of delegates: the loop holds the call IDs.
    state.delegate_wait_calls = vec![
        crate::quiesce::DelegateWaitCall {
            tool_call_id: "call-a".into(),
            child: first,
        },
        crate::quiesce::DelegateWaitCall {
            tool_call_id: "call-b".into(),
            child: second,
        },
    ];
    crate::meta::wait_delegates(&mut state, &runtime, vec![Ok(first), Ok(second)]).await;
    state.delegate_wait_calls.clear();
    let wave = transitions(events.drain_pending_snapshot(), parent);

    // One direct delegate: the dispatcher's call ID is the only identity.
    state.meta_call_id = Some("call-single".into());
    crate::meta::wait_delegates(&mut state, &runtime, vec![Ok(first)]).await;
    let single = transitions(events.drain_pending_snapshot(), parent);

    assert_eq!(
        wave[0],
        (
            AgentRunPhase::Waiting,
            None,
            Some(WaitingCause::Delegates {
                delegates: vec![
                    event_bus::WaitingDelegate {
                        call_id: "call-a".into(),
                        run_id: first.to_string()
                    },
                    event_bus::WaitingDelegate {
                        call_id: "call-b".into(),
                        run_id: second.to_string()
                    },
                ]
            })
        )
    );
    assert_eq!(wave[1], (AgentRunPhase::Running, None, None));
    assert_eq!(
        single[0].2,
        Some(WaitingCause::Delegates {
            delegates: vec![event_bus::WaitingDelegate {
                call_id: "call-single".into(),
                run_id: first.to_string()
            }]
        })
    );
}

#[test]
fn the_state_machine_refuses_a_waiting_transition_without_a_cause() {
    let mut run = crate::state::RunState::new();
    run.transition(RunId::new(1), AgentRunPhase::Running, None)
        .unwrap();

    assert!(
        run.transition(RunId::new(1), AgentRunPhase::Waiting, None)
            .is_err()
    );
    assert_eq!(run.phase(), AgentRunPhase::Running);
    let event = run
        .transition_waiting(RunId::new(1), WaitingCause::InputBoundary)
        .unwrap();
    assert!(matches!(
        event,
        LifecycleEvent::AgentRunStateChanged {
            reason: None,
            waiting: Some(WaitingCause::InputBoundary),
            ..
        }
    ));
    let resumed = run
        .transition(RunId::new(1), AgentRunPhase::Running, None)
        .unwrap();
    assert!(matches!(
        resumed,
        LifecycleEvent::AgentRunStateChanged { waiting: None, .. }
    ));
}

#[tokio::test]
async fn a_delivery_error_never_leaves_the_run_waiting_and_keeps_the_message_queued() {
    // Saving the oversized message's artifact fails below a regular file, so the
    // production delivery path itself reports the error; no hook is injected.
    const CHILD: &str = "EVORCH_WAITING_DELIVER_ERROR_TEST";
    let test = "agent_loop::waiting_cause_tests::a_delivery_error_never_leaves_the_run_waiting_and_keeps_the_message_queued";
    if std::env::var(CHILD).is_err() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("file");
        std::fs::write(&file, "x").unwrap();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test, "--nocapture"])
            .env(CHILD, "1")
            .env("EVORCH_OUTPUT_DIR", file.join("below"))
            .status()
            .unwrap();
        assert!(status.success(), "isolated process failed");
        return;
    }
    let (_cancel, cancel_rx) = watch::channel(RunInterrupt::None);
    let (runtime, mut state) = fixture(cancel_rx);
    state.transition(AgentRunPhase::Running, None).unwrap();
    let parent = state.task.run_id;
    let child = terminal_child(&runtime, parent);
    runtime
        .send_agent_message(
            child,
            parent,
            AgentMessageKind::Send,
            "x".repeat(100_000),
            None,
        )
        .unwrap();
    let mailbox = runtime.run_mailbox(parent).unwrap();
    let queued = mailbox.len();
    let mut events = state.shared.bus.subscribe();

    let result = crate::meta::dispatch(
        &mut state,
        "wait",
        serde_json::json!({"run_id": child.to_string()}),
    )
    .await;

    assert!(result.result.is_error, "{:?}", result.result.content);
    assert_eq!(
        transitions(events.drain_pending_snapshot(), parent)
            .into_iter()
            .map(|(phase, _, waiting)| (phase, waiting.is_some()))
            .collect::<Vec<_>>(),
        [
            (AgentRunPhase::Waiting, true),
            (AgentRunPhase::Running, false)
        ]
    );
    assert_eq!(mailbox.len(), queued);
}
