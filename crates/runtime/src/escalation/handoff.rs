//! 終端済み Direct run から新規 Orchestrator root run への所有権移譲。

use std::sync::Weak;

use event_bus::event::diagnostic_codes;
use event_bus::{AgentRunPhase, DiagnosticEvent, DiagnosticSeverity, Event};

use super::EscalationMemo;
use crate::agent_loop::LoopState;
use crate::runtime::Shared;

pub(crate) async fn complete(shared: &Weak<Shared>, state: &mut LoopState, memo: EscalationMemo) {
    match state.phase() {
        AgentRunPhase::Done | AgentRunPhase::Error => {}
        // Operator stop is resumable, not a completed escalation.
        AgentRunPhase::Stopped => return,
        AgentRunPhase::Pending | AgentRunPhase::Running | AgentRunPhase::Waiting => {
            tracing::warn!(
                source_run_id = %memo.source_run_id,
                "escalation handoff rejected before terminal state"
            );
            return;
        }
    }
    let Some(runtime) = crate::AgentRuntime::from_weak(shared) else {
        return;
    };
    let source_run_id = memo.source_run_id;
    let config = state.run_config().clone();
    let frozen = state
        .continuity
        .freeze(&runtime, source_run_id, &state.context.messages);
    let spawned = frozen.and_then(|continuity| {
        runtime.spawn_escalated_root(memo, &config, continuity, || {
            state.publish_terminal();
        })
    });
    match spawned {
        Ok(_) => {}
        Err(reason) => {
            runtime.goal_work_stopped(source_run_id);
            state.shared.bus.emit(Event::new(DiagnosticEvent {
                source: "escalation_handoff".into(),
                severity: DiagnosticSeverity::Error,
                code: diagnostic_codes::ESCALATION_HANDOFF_FAILED.into(),
                detail: format!("escalation preparation failed: {reason}"),
                run_id: Some(source_run_id.to_string()),
                thread_id: None,
                call_id: None,
            }));
        }
    }
}
