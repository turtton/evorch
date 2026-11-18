use super::*;
use crate::agent_loop::delegate_cleanup_tests::started_fixture;
use crate::{InterruptKind, RunInterrupt};
use event_bus::{AgentMessage, AgentMessageKind, AgentRunPhase};
use tokio::sync::mpsc;

async fn ready_wake(
    state: &mut LoopState,
    source: usize,
    cancel: &watch::Sender<RunInterrupt>,
) -> InputWake {
    let (quiesce, mut quiesce_rx) = watch::channel(0);
    let (deliveries, mut deliveries_rx) = watch::channel(0);
    let (answers, mut answers_rx) = watch::channel(0);
    // The fixture's original sender is closed, which would mask mailbox readiness.
    let (inbox, receiver) = mpsc::channel(1);
    state.channels.inbox_rx = receiver.into();
    match source {
        0 => {
            quiesce.send_replace(1);
        }
        1 => {
            cancel.send_replace(RunInterrupt::Cancel);
        }
        2 => {
            deliveries.send_replace(1);
        }
        3 => {
            answers.send_replace(1);
        }
        4 => {
            inbox.send(("notice".into(), false, None, 0)).await.unwrap();
        }
        5 => {
            state
                .task
                .mailbox
                .try_push(AgentMessage {
                    message_id: "wake".into(),
                    sender_run_id: "sender".into(),
                    recipient_run_id: state.task.run_id.to_string(),
                    kind: AgentMessageKind::Send,
                    content: "message".into(),
                    reply_to: None,
                })
                .unwrap();
        }
        _ => unreachable!(),
    }
    state
        .next_input_wake(&mut quiesce_rx, &mut deliveries_rx, &mut answers_rx)
        .await
}

#[tokio::test]
async fn every_boundary_wake_is_adopted_until_running_publication() {
    for source in 0..6 {
        let (cancel, cancel_rx) = watch::channel(RunInterrupt::None);
        let (runtime, mut state) = started_fixture(cancel_rx).await;
        let run = state.caller_run_id();
        state.transition(AgentRunPhase::Running, None).unwrap();
        state
            .transition_with_turn(AgentRunPhase::Waiting, None, Some(None))
            .unwrap();
        let wake = ready_wake(&mut state, source, &cancel).await;
        assert!(matches!(
            (source, wake),
            (0, InputWake::Quiesce)
                | (1, InputWake::Cancel(Ok(())))
                | (2, InputWake::Deliveries(Ok(())))
                | (3, InputWake::Answers(Ok(())))
                | (4, InputWake::Inbox(Some(_)))
                | (5, InputWake::Mailbox(Ok(())))
        ));
        let inbox = state.channels.user_inbox.clone();
        assert!(inbox.adopting());
        assert_eq!(*state.channels.phase_tx.borrow(), AgentRunPhase::Waiting);
        state.transition(AgentRunPhase::Running, None).unwrap();
        assert_eq!(*state.channels.phase_tx.borrow(), AgentRunPhase::Running);
        assert!(!inbox.adopting());
        drop(state);
        runtime.cancel(run).unwrap();
        runtime.wait(run).await.unwrap();
    }
}

#[tokio::test]
async fn failed_publication_error_cancel_and_abandonment_release_the_common_guard() {
    enum Finish {
        Abandon,
        Error,
        Cancel,
    }
    for terminal in [Finish::Abandon, Finish::Error, Finish::Cancel] {
        let (cancel, cancel_rx) = watch::channel(RunInterrupt::None);
        let (runtime, mut state) = started_fixture(cancel_rx).await;
        let run = state.caller_run_id();
        let inbox = state.channels.user_inbox.clone();
        assert!(matches!(
            ready_wake(&mut state, 5, &cancel).await,
            InputWake::Mailbox(Ok(()))
        ));
        // Pending -> Waiting is invalid: retain the bridge until teardown.
        assert!(state.transition(AgentRunPhase::Waiting, None).is_err());
        match terminal {
            Finish::Error => state.finish_error("wake failed".into()),
            Finish::Cancel => state.finish_interrupted(InterruptKind::Cancel),
            Finish::Abandon => {}
        }
        assert!(inbox.adopting());
        drop(state);
        assert!(!inbox.adopting());
        runtime.cancel(run).unwrap();
        runtime.wait(run).await.unwrap();
    }
}

#[tokio::test]
async fn an_ignored_wake_releases_when_the_loop_waits_again() {
    use std::{future::Future, task::Context};
    let (cancel, cancel_rx) = watch::channel(RunInterrupt::None);
    let (runtime, mut state) = started_fixture(cancel_rx).await;
    let run = state.caller_run_id();
    assert!(matches!(
        ready_wake(&mut state, 5, &cancel).await,
        InputWake::Mailbox(Ok(()))
    ));
    let inbox = state.channels.user_inbox.clone();
    assert!(inbox.adopting());
    state.task.mailbox.drain_queued_where(|_| true);
    let (_sender, receiver) = mpsc::channel(1);
    state.channels.inbox_rx = receiver.into();
    let (_q, mut quiesce) = watch::channel(0);
    let (_d, mut deliveries) = watch::channel(0);
    let (_a, mut answers) = watch::channel(0);
    // Draining advances the mailbox version even when nothing remains. Handle
    // that no-op notification before returning to a truly passive wait.
    assert!(matches!(
        state
            .next_input_wake(&mut quiesce, &mut deliveries, &mut answers)
            .await,
        InputWake::Mailbox(Ok(()))
    ));
    assert!(inbox.adopting());
    let mut next = Box::pin(state.next_input_wake(&mut quiesce, &mut deliveries, &mut answers));
    let mut cx = Context::from_waker(std::task::Waker::noop());
    assert!(next.as_mut().poll(&mut cx).is_pending());
    assert!(!inbox.adopting());
    drop(next);
    assert!(
        !inbox.adopting(),
        "cancelling the wait must not leak a guard"
    );
    drop(state);
    runtime.cancel(run).unwrap();
    runtime.wait(run).await.unwrap();
}

mod public_gate;
