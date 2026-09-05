//! 終端済み Direct run から新規 Orchestrator root run への所有権移譲。

use std::sync::Weak;

use event_bus::{AgentRunPhase, Event, LifecycleEvent};

use super::EscalationMemo;
use crate::agent_loop::{LoopState, cleanup_worktree};
use crate::runtime::Shared;
use crate::workspace::OwnedWorktree;

pub(crate) async fn complete(
    shared: &Weak<Shared>,
    state: &LoopState,
    memo: EscalationMemo,
    worktree: Option<OwnedWorktree>,
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
    let summary = memo.summary();
    let new_run_id = runtime.spawn_escalated_root(memo, state.run_config(), worktree);
    state
        .shared
        .bus
        .emit(Event::new(LifecycleEvent::EscalationRequested {
            source_run_id: source_run_id.to_string(),
            new_run_id: new_run_id.to_string(),
            summary,
        }));
}
