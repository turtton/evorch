use super::*;
use crate::mailbox::CompletionNotice;
use event_bus::{AgentMessage, AgentMessageKind};
use futures_util::poll;
use tokio::sync::mpsc;

fn message(parent: RunId, child: RunId) -> AgentMessage {
    AgentMessage {
        message_id: "notice".into(),
        sender_run_id: child.to_string(),
        recipient_run_id: parent.to_string(),
        kind: AgentMessageKind::Send,
        content: "same child output".into(),
        reply_to: None,
    }
}

#[tokio::test]
async fn observed_only_aside_is_not_input_or_a_history_rewrite() {
    let (_cancel, cancel_rx) = watch::channel(RunInterrupt::None);
    let (_runtime, mut state) = super::delegate_cleanup_tests::fixture(cancel_rx);
    let parent = state.task.run_id;
    let child = RunId::new(99);
    let notice = CompletionNotice::new(Some(parent), child, 1);
    state
        .task
        .mailbox
        .try_push_completion(message(parent, child), notice.clone())
        .unwrap();
    notice.observe();
    let before = state.context.messages.clone();
    assert!(!state.flush_aside());
    assert!(!state.resumed);
    assert_eq!(state.context.messages, before);
}

#[tokio::test]
async fn waiting_ignores_observed_notices_but_delivers_explicit_unobserved_and_new_generations() {
    for control in ["send", "reply", "steering", "unobserved", "new_generation"] {
        let (_cancel, cancel_rx) = watch::channel(RunInterrupt::None);
        let (_runtime, mut state) = super::delegate_cleanup_tests::fixture(cancel_rx);
        let (_inbox, inbox_rx) = mpsc::channel(1);
        state.channels.inbox_rx = inbox_rx.into();
        state.transition(AgentRunPhase::Running, None).unwrap();
        let parent = state.task.run_id;
        let child = RunId::new(99);
        let mailbox = state.task.mailbox.clone();
        let phase = state.channels.phase_tx.subscribe();
        let old = CompletionNotice::new(Some(parent), child, 1);
        let waiting = state.wait_for_input();
        tokio::pin!(waiting);
        assert!(poll!(&mut waiting).is_pending());
        mailbox
            .try_push_completion(message(parent, child), old.clone())
            .unwrap();
        old.observe();
        // The version has changed, but the only queued notice is now observed.
        assert!(poll!(&mut waiting).is_pending());
        assert_eq!(*phase.borrow(), AgentRunPhase::Waiting);
        match control {
            "unobserved" | "new_generation" => {
                let child = if control == "unobserved" {
                    RunId::new(100)
                } else {
                    child
                };
                mailbox
                    .try_push_completion(
                        message(parent, child),
                        CompletionNotice::new(Some(parent), child, 2),
                    )
                    .unwrap();
            }
            _ => {
                let mut explicit = message(parent, child);
                explicit.kind = match control {
                    "reply" => AgentMessageKind::Reply,
                    "steering" => AgentMessageKind::Steering,
                    _ => AgentMessageKind::Send,
                };
                mailbox.try_push(explicit).unwrap();
            }
        }
        assert!(waiting.await);
        assert_eq!(*phase.borrow(), AgentRunPhase::Running);
    }
}

#[tokio::test]
async fn phase_only_wait_leaves_notice_but_output_bearing_wait_acknowledges_it() {
    let (_cancel, cancel_rx) = watch::channel(RunInterrupt::None);
    let (runtime, mut state) = super::delegate_cleanup_tests::fixture(cancel_rx);
    state.transition(AgentRunPhase::Running, None).unwrap();
    let parent = state.task.run_id;
    let child = runtime
        .delegate_background_as_child(parent, Role::Worker, "child", RunConfig::default())
        .unwrap();
    runtime.publish_terminal(
        child,
        event_bus::LifecycleEvent::AgentRunStateChanged {
            run_id: child.to_string(),
            from: AgentRunPhase::Running,
            to: AgentRunPhase::Done,
            reason: None,
        },
    );
    let result = crate::meta::dispatch(
        &mut state,
        "wait",
        serde_json::json!({"run_id":child.to_string()}),
    )
    .await;
    assert!(!result.result.is_error);
    assert_eq!(result.result.content, "Done");
    assert_eq!(runtime.run_mailbox(parent).unwrap().len(), 1);
    let result = crate::meta::dispatch(
        &mut state,
        "wait",
        serde_json::json!({"run_ids":[child.to_string()]}),
    )
    .await;
    assert!(!result.result.is_error);
    assert!(runtime.run_mailbox(parent).unwrap().is_empty());
}
