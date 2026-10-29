use event_bus::{DiagnosticEvent, DiagnosticSeverity, Event, event::diagnostic_codes};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::LoopState;
use super::stop_context::{ToolOutcome, stop_reason};
use crate::escalation::detector::ToolErrorClass;

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
    /// Calls made inside `run_js` programs. Kept apart from `calls` so that the
    /// children between two identical `run_js` calls do not hide the repeat.
    children: Streak,
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
            if name != tools::tools::run_js::NAME {
                self.children.reset();
            }
            if reports_progress(name, input) {
                self.calls.reset();
            } else if self.calls.push(name, input) >= limit {
                return Some(call);
            }
        }
        None
    }

    /// Records a call a `run_js` program made. Returns true when the program (or
    /// consecutive `run_js` calls) made `limit` identical calls in a row.
    pub(super) fn observe_child(&mut self, name: &str, input: &Value, limit: u32) -> bool {
        self.children.push(name, input) >= limit
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
    /// Calls judged by their results rather than their inputs: an explicit
    /// shell poll, and `wait`, which blocks until something happens.
    pub(super) fn reports_progress(&self, name: &str, input: &Value) -> bool {
        name == "wait" || self.shared.executor.reports_progress(name, input)
    }

    /// Records the result of a call that reports progress. Returns true when
    /// identical calls stalled the configured number of times in a row.
    pub(super) fn observe_progress(
        &mut self,
        name: &str,
        input: &Value,
        result: &tools::ToolResult,
    ) -> bool {
        let limit = self.task.config.budget.max_identical_tool_call_repeats;
        // Failed, denied or cancelled calls never count as progress.
        let advanced = !result.is_error && result.progress == Some(tools::Progress::Advanced);
        self.identical_calls
            .observe_result(name, input, advanced, limit)
    }

    pub(super) fn guard_identical_calls(&mut self, calls: &[(String, String, Value)]) -> bool {
        let limit = self.task.config.budget.max_identical_tool_call_repeats;
        let executor = std::sync::Arc::clone(&self.shared.executor);
        let Some((call_id, name, input)) = self.identical_calls.observe(
            calls,
            |name, input| name == "wait" || executor.reports_progress(name, input),
            limit,
        ) else {
            return true;
        };
        // Any other call ends the streak, so a last result of this tool is the repeated call's.
        let error = self
            .last_tool
            .as_ref()
            .filter(|last| last.tool == *name)
            .and_then(|last| last.error);
        self.stop_identical_calls(call_id, name, input, error, false);
        false
    }

    /// Stops the run after `limit` identical calls in a row; `stalled` marks
    /// calls counted because their results showed no progress. `error` is the
    /// class of the repeated call's last result when it failed.
    pub(super) fn stop_identical_calls(
        &mut self,
        call_id: &str,
        name: &str,
        input: &Value,
        error: Option<ToolErrorClass>,
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
        self.finish_error(stop_reason(
            code,
            Some(&ToolOutcome {
                tool: name.to_owned(),
                error,
            }),
            &detail,
        ));
    }
}
