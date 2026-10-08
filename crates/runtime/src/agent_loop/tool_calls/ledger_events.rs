//! Meta operations bypass ToolExecutor; the ones the user should see in the
//! conversation (ledger access, skill loads, goal creation) still share the
//! GUI tool lifecycle.

use event_bus::{Event, ToolEvent};
use serde_json::Value;
use tools::ToolResult;

use super::LoopState;

/// Meta operations reported as ordinary tool calls in the transcript.
const VISIBLE_META_OPS: &[&str] = &["ledger_append", "ledger_read", "skill_load", "create_goal"];

impl LoopState {
    pub(super) fn ledger_tool_started(&self, name: &str, id: &str, input: &Value) {
        if VISIBLE_META_OPS.contains(&name) {
            self.shared.bus.emit(Event::new(ToolEvent::ToolStarted {
                tool_name: name.into(),
                call_id: id.into(),
                input: Some(input.clone()),
                run_id: Some(self.caller_run_id().to_string()),
            }));
        }
    }

    pub(super) fn ledger_tool_completed(&self, name: &str, id: &str, result: &ToolResult) {
        if VISIBLE_META_OPS.contains(&name) {
            self.shared.bus.emit(Event::new(ToolEvent::ToolCompleted {
                tool_name: name.into(),
                call_id: id.into(),
                is_error: result.is_error,
                output: Some(result.content.clone()),
                detail: result.detail.clone(),
                run_id: Some(self.caller_run_id().to_string()),
            }));
        }
    }
}
