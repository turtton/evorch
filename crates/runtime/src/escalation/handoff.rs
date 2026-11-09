//! Publication/start half of an already committed handoff. Preparation belongs
//! to the tool coordinator, never terminal teardown.
use super::EscalationMemo;
use crate::{agent_loop::LoopState, runtime::Shared};
use event_bus::{Event, LifecycleEvent};
use std::sync::Weak;
pub(crate) async fn complete(shared: &Weak<Shared>, state: &mut LoopState, memo: EscalationMemo) {
    let incarnation = state.task.publication.incarnation();
    state.publish_terminal();
    let Some(runtime) = crate::AgentRuntime::from_weak(shared) else {
        return;
    };
    let config = state.run_config().clone();
    if let Some(store) = runtime.shared.run_store.get()
        && let Ok(Some(receipt)) = store
            .handle
            .handoff_operation(memo.source_run_id.to_string(), incarnation.clone())
        && receipt.state.is_committed()
    {
        runtime
            .shared
            .bus
            .emit(Event::new(LifecycleEvent::EscalationRequested {
                source_run_id: memo.source_run_id.to_string(),
                new_run_id: receipt.target_run_id,
                summary: memo.summary(),
            }));
    }
    if let Err(reason) = runtime.start_committed_handoff(memo.source_run_id, &incarnation, &config)
    {
        runtime.lane_delivery_diagnostic(
            Some(memo.source_run_id),
            "handoff_start_pending",
            &reason,
        );
    }
}
