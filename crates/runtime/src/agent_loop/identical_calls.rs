use event_bus::{DiagnosticEvent, DiagnosticSeverity, Event, event::diagnostic_codes};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::LoopState;

/// Consecutive repeats of one `(name, input)`.
#[derive(Default)]
struct Streak {
    last: Option<(String, Value)>,
    count: u32,
}

impl Streak {
    fn push(&mut self, name: &str, input: &Value) -> u32 {
        if self
            .last
            .as_ref()
            .is_some_and(|(last_name, last_input)| last_name == name && last_input == input)
        {
            self.count = self.count.saturating_add(1);
        } else {
            self.last = Some((name.to_owned(), input.clone()));
            self.count = 1;
        }
        self.count
    }

    fn reset(&mut self) {
        *self = Self::default();
    }
}

/// Calls that report progress are judged by their results after execution;
/// every other call is judged by its input before the batch runs. Each kind
/// of call ends the other kind's streak.
#[derive(Default)]
pub(super) struct IdenticalCalls {
    calls: Streak,
    stalled: Streak,
}

impl IdenticalCalls {
    fn observe<'a>(
        &mut self,
        calls: &'a [(String, String, Value)],
        reports_progress: impl Fn(&str, &Value) -> bool,
        limit: u32,
    ) -> Option<&'a (String, String, Value)> {
        if calls.is_empty() {
            self.calls.reset();
            self.stalled.reset();
        }
        for call @ (_, name, input) in calls {
            if reports_progress(name, input) {
                self.calls.reset();
            } else if self.calls.push(name, input) >= limit {
                return Some(call);
            }
        }
        None
    }

    /// Ends the stalled streak at a call that does not report progress.
    pub(super) fn end_stalled(&mut self) {
        self.stalled.reset();
    }

    /// Records an executed call that reports progress, in input order. Returns
    /// true when identical calls stalled `limit` times in a row.
    pub(super) fn observe_result(
        &mut self,
        name: &str,
        input: &Value,
        advanced: bool,
        limit: u32,
    ) -> bool {
        self.stalled.push(name, input);
        if advanced {
            self.stalled.count = 0;
        }
        self.stalled.count >= limit
    }
}

/// First 16 hex digits of the SHA-256 of the canonical JSON input.
fn input_digest(input: &Value) -> String {
    Sha256::digest(input.to_string().as_bytes())
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

impl LoopState {
    pub(super) fn guard_identical_calls(&mut self, calls: &[(String, String, Value)]) -> bool {
        let limit = self.task.config.budget.max_identical_tool_call_repeats;
        let executor = std::sync::Arc::clone(&self.shared.executor);
        let Some((call_id, name, input)) = self.identical_calls.observe(
            calls,
            |name, input| executor.reports_progress(name, input),
            limit,
        ) else {
            return true;
        };
        self.stop_identical_calls(call_id, name, input, false);
        false
    }

    /// Stops the run after `limit` identical calls in a row; `stalled` marks
    /// calls counted because their results showed no progress.
    pub(super) fn stop_identical_calls(
        &mut self,
        call_id: &str,
        name: &str,
        input: &Value,
        stalled: bool,
    ) {
        let limit = self.task.config.budget.max_identical_tool_call_repeats;
        let code = diagnostic_codes::IDENTICAL_TOOL_CALLS;
        let kind = if stalled { " without progress" } else { "" };
        let detail = format!(
            "consecutive identical tool calls{kind} reached max_identical_tool_call_repeats={limit}"
        );
        self.shared.bus.emit(Event::new(DiagnosticEvent {
            source: "budget_tracker".into(),
            severity: DiagnosticSeverity::Error,
            code: code.into(),
            // A digest groups repeats of one input without persisting possibly secret arguments.
            detail: format!("{detail}\ntool={name} input_sha256={}", input_digest(input)),
            run_id: Some(self.task.run_id.to_string()),
            thread_id: None,
            call_id: Some(call_id.to_owned()),
        }));
        self.finish_error(format!("{code}: {detail}"));
    }
}
