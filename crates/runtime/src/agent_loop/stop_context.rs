//! Short context for the reasons of runs stopped by a loop guard.
//!
//! A parent judges a stopped child from its reason alone, often truncated, so
//! the reason names the tool involved and how it last failed right after the
//! stop code. Like the escalation evidence, only fixed class names are used:
//! tool inputs and error texts can echo arguments and never enter a reason.

use tools::ToolResult;

use super::LoopState;
use crate::escalation::detector::ToolErrorClass;

/// Longest tool name copied into a reason; names come from the model.
const MAX_TOOL_NAME_CHARS: usize = 64;

/// The tool of the last recorded tool result and, when it failed, its class.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ToolOutcome {
    pub(super) tool: String,
    pub(super) error: Option<ToolErrorClass>,
}

/// `{code}: tool=<name>[ last_error=<class>]; {detail}`, or `{code}: {detail}`
/// without a tool. The code stays first so readers matching on it keep working.
pub(super) fn stop_reason(code: &str, outcome: Option<&ToolOutcome>, detail: &str) -> String {
    let Some(outcome) = outcome else {
        return format!("{code}: {detail}");
    };
    let tool = reason_tool_name(&outcome.tool);
    match outcome.error {
        Some(error) => format!(
            "{code}: tool={tool} last_error={}; {detail}",
            error.as_str()
        ),
        None => format!("{code}: tool={tool}; {detail}"),
    }
}

/// The name with characters outside tool-name syntax replaced, so a model-made
/// name cannot add fields or lines to the reason, and capped in length.
fn reason_tool_name(name: &str) -> String {
    name.chars()
        .take(MAX_TOOL_NAME_CHARS)
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.' | ':') {
                ch
            } else {
                '?'
            }
        })
        .collect()
}

impl LoopState {
    /// Records `result` as the run's last tool result and returns its failure
    /// class. A typed error keeps its class; an error text is classified by its
    /// fixed prefix only.
    pub(super) fn record_tool_outcome(
        &mut self,
        tool: &str,
        result: &ToolResult,
        typed: Option<ToolErrorClass>,
    ) -> Option<ToolErrorClass> {
        let error = result
            .is_error
            .then(|| typed.unwrap_or_else(|| ToolErrorClass::from_content(&result.content)));
        self.last_tool = Some(ToolOutcome {
            tool: tool.to_owned(),
            error,
        });
        error
    }
}

#[cfg(test)]
mod tests {
    use super::{ToolOutcome, stop_reason};
    use crate::escalation::detector::ToolErrorClass;

    fn outcome(tool: &str, error: Option<ToolErrorClass>) -> ToolOutcome {
        ToolOutcome {
            tool: tool.into(),
            error,
        }
    }

    #[test]
    fn a_failed_last_result_adds_its_class_after_the_tool() {
        let failed = outcome("edit", Some(ToolErrorClass::InvalidArgs));
        assert_eq!(
            stop_reason("NoProgress", Some(&failed), "detail"),
            "NoProgress: tool=edit last_error=invalid_args; detail"
        );
    }

    #[test]
    fn a_successful_last_result_names_only_the_tool() {
        assert_eq!(
            stop_reason("NoProgress", Some(&outcome("read", None)), "detail"),
            "NoProgress: tool=read; detail"
        );
        assert_eq!(
            stop_reason("NoProgress", None, "detail"),
            "NoProgress: detail"
        );
    }

    #[test]
    fn a_model_made_tool_name_cannot_add_fields_or_grow_the_reason() {
        let name = format!("x last_error=none;\n{}", "y".repeat(100));
        let reason = stop_reason("NoProgress", Some(&outcome(&name, None)), "detail");
        assert_eq!(
            reason,
            format!(
                "NoProgress: tool=x?last_error?none??{}; detail",
                "y".repeat(45)
            )
        );
    }
}
