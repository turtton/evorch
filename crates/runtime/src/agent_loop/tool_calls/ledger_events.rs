//! Ledger meta operations bypass ToolExecutor but share the GUI tool lifecycle.

use event_bus::{Event, ToolEvent};
use serde_json::Value;
use tools::ToolResult;

use super::LoopState;

impl LoopState {
    pub(super) fn ledger_tool_started(&self, name: &str, id: &str, input: &Value) {
        if matches!(name, "ledger_append" | "ledger_read") {
            self.shared.bus.emit(Event::new(ToolEvent::ToolStarted {
                tool_name: name.into(),
                call_id: id.into(),
                input: Some(input.clone()),
                run_id: Some(self.caller_run_id().to_string()),
            }));
        }
    }

    pub(super) fn ledger_tool_completed(&self, name: &str, id: &str, result: &ToolResult) {
        if matches!(name, "ledger_append" | "ledger_read") {
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
