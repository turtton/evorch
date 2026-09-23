//! 終端済み Direct run から新規 Orchestrator root run への所有権移譲。

use std::sync::Weak;

use event_bus::{AgentRunPhase, DiagnosticEvent, DiagnosticSeverity, Event};

use super::EscalationMemo;
use crate::agent_loop::{LoopState, cleanup_worktree};
use crate::runtime::Shared;
use crate::workspace::OwnedWorktree;

pub(crate) async fn complete(
    shared: &Weak<Shared>,
    state: &mut LoopState,
    memo: EscalationMemo,
    mut worktree: Option<OwnedWorktree>,
) {
    match state.phase() {
        AgentRunPhase::Done | AgentRunPhase::Error => {}
        AgentRunPhase::Pending | AgentRunPhase::Running | AgentRunPhase::Waiting => {
            tracing::warn!(
                source_run_id = %memo.source_run_id,
                "escalation handoff rejected before terminal state"
            );
            cleanup_worktree(&state.shared, state.caller_run_id(), worktree).await;
            return;
        }
    }
    let Some(runtime) = crate::AgentRuntime::from_weak(shared) else {
        cleanup_worktree(&state.shared, state.caller_run_id(), worktree).await;
        return;
    };
    let source_run_id = memo.source_run_id;
    let config = state.run_config().clone();
    match runtime.spawn_escalated_root(memo, &config, &mut worktree, || {
        state.publish_terminal();
    }) {
        Ok(_) => {}
        Err(reason) => {
            state.shared.bus.emit(Event::new(DiagnosticEvent {
                source: "escalation_handoff".into(),
                severity: DiagnosticSeverity::Error,
                code: "EscalationHandoffFailed".into(),
                detail: format!("question inheritance failed: {reason}"),
                run_id: Some(source_run_id.to_string()),
                thread_id: None,
                call_id: None,
            }));
            cleanup_worktree(&state.shared, source_run_id, worktree).await;
        }
    }
}
