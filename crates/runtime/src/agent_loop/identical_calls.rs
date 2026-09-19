use event_bus::{DiagnosticEvent, DiagnosticSeverity, Event, event::diagnostic_codes};
use serde_json::Value;

use super::LoopState;

#[derive(Default)]
pub(super) struct IdenticalCalls {
    last: Option<(String, Value)>,
    count: u32,
}

impl IdenticalCalls {
    fn observe<'a>(&mut self, calls: &'a [(String, String, Value)], limit: u32) -> Option<&'a str> {
        if calls.is_empty() {
            self.last = None;
            self.count = 0;
        }
        for (id, name, input) in calls {
            if self
                .last
                .as_ref()
                .is_some_and(|(last_name, last_input)| last_name == name && last_input == input)
            {
                self.count = self.count.saturating_add(1);
            } else {
                self.last = Some((name.clone(), input.clone()));
                self.count = 1;
            }
            if self.count >= limit {
                return Some(id);
            }
        }
        None
    }
}

impl LoopState {
    pub(super) fn guard_identical_calls(&mut self, calls: &[(String, String, Value)]) -> bool {
        let limit = self.task.config.budget.max_identical_tool_call_repeats;
        let Some(call_id) = self.identical_calls.observe(calls, limit) else {
            return true;
        };
        let code = diagnostic_codes::IDENTICAL_TOOL_CALLS;
        let detail = format!(
            "consecutive identical tool calls reached max_identical_tool_call_repeats={limit}"
        );
        self.shared.bus.emit(Event::new(DiagnosticEvent {
            source: "budget_tracker".into(),
            severity: DiagnosticSeverity::Error,
            code: code.into(),
            detail: detail.clone(),
            run_id: Some(self.task.run_id.to_string()),
            thread_id: None,
            call_id: Some(call_id.into()),
        }));
        self.finish_error(format!("{code}: {detail}"));
        false
    }
}
