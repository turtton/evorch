//! Calls the runtime handles itself bypass ToolExecutor, so it reports their
//! lifecycle to the GUI. Every meta operation is visible except `finish`,
//! whose result is already published as the run's final answer.

use event_bus::{Event, ToolEvent};
use serde_json::Value;
use tools::ToolResult;

use super::LoopState;
use crate::policy::is_meta_op;

/// Runtime-owned team-ledger calls that are not meta operations.
const TEAM_OPS: &[&str] = &["task_claim", "task_complete", "finding_append"];

/// Whether the runtime reports `name` as an ordinary tool call in the transcript.
pub(super) fn is_visible_op(name: &str) -> bool {
    (is_meta_op(name) && name != "finish") || TEAM_OPS.contains(&name)
}

impl LoopState {
    pub(super) fn ledger_tool_started(&self, name: &str, id: &str, input: &Value) {
        if is_visible_op(name) {
            self.shared.bus.emit(Event::new(ToolEvent::ToolStarted {
                tool_name: name.into(),
                call_id: id.into(),
                input: Some(input.clone()),
                run_id: Some(self.caller_run_id().to_string()),
                parent_call_id: None,
            }));
        }
    }

    pub(super) fn ledger_tool_completed(&self, name: &str, id: &str, result: &ToolResult) {
        if is_visible_op(name) {
            self.shared.bus.emit(Event::new(ToolEvent::ToolCompleted {
                tool_name: name.into(),
                call_id: id.into(),
                is_error: result.is_error,
                output: Some(result.content.clone()),
                detail: result.detail.clone(),
                run_id: Some(self.caller_run_id().to_string()),
                parent_call_id: None,
            }));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::is_visible_op;
    use crate::policy::META_OPS;

    #[test]
    fn every_meta_operation_but_finish_is_reported_to_the_transcript() {
        // Given: the full registry of runtime-owned operations.
        // When/Then: only `finish` stays hidden, so new operations default to visible.
        for name in META_OPS {
            assert_eq!(is_visible_op(name), *name != "finish", "{name}");
        }
        for name in [
            "delegate",
            "send_message",
            "escalate",
            "todo_write",
            "get_goal",
        ] {
            assert!(is_visible_op(name), "{name}");
        }
        assert!(is_visible_op("task_claim"));
        assert!(!is_visible_op("read"), "executor tools report themselves");
    }
}
