//! Conversation recovery is not tool replay. Keep the saved provider prefix intact
//! and append an outcome-unknown error for calls whose results could not be saved.
use super::{InterruptedToolCall, RestoredState, RunRestoreDescriptor};
use providers::{ContentBlock, Message, Role};
use storage::RunContextRecord;

pub(super) const UNRESOLVED_TOOL_CALLS_REASON: &str =
    "unresolved_tool_calls: inspect actual effects before starting a new run";

const OUTCOME_UNKNOWN_NOTICE: &str = "The execution result was not retained in the recoverable history, or execution was interrupted. \
    The operation may have partially or fully executed; neither success nor absence of effects is established. \
    Conversation history has been retained. The runtime has not replayed this operation or resumed its old shell jobs. \
    Continue responding to the user. If further work depends on this outcome, inspect the current state before deciding whether to retry; do not blindly repeat the operation.";

impl RestoredState {
    /// Only chat entrances with freshly validated authority may use this path.
    /// Missing batches were omitted at checkpoint time: do not fabricate ToolUse
    /// arguments or orphan ToolResult blocks for them. Existing results (including
    /// cancellation errors) are immutable; report the uncertainty as new context.
    pub(crate) fn for_conversation(record: &RunContextRecord) -> Result<Self, crate::RuntimeError> {
        let descriptor: RunRestoreDescriptor =
            serde_json::from_str(&record.config_json).map_err(|error| {
                crate::RuntimeError::RunRestoreFailed {
                    run_id: record.run_id.clone(),
                    reason: crate::RunRestoreFailure::CorruptContext(error.to_string()),
                }
            })?;
        let mut history_record = record.clone();
        let mut history_descriptor = descriptor.clone();
        history_descriptor.interrupted_tool_calls.clear();
        history_record.config_json =
            serde_json::to_string(&history_descriptor).expect("restore descriptor is serializable");
        let mut restored = Self::from_record(&history_record)?;
        for call in &descriptor.interrupted_tool_calls {
            if call.call_id == "unobserved-shell-jobs"
                || !has_outcome_notice(&restored.messages, call)
            {
                restored.messages.push(Message {
                    role: Role::User,
                    content: vec![ContentBlock::Text {
                        text: outcome_notice(call),
                    }],
                });
            }
        }
        Ok(restored)
    }
}

fn outcome_notice(call: &InterruptedToolCall) -> String {
    let identity = serde_json::json!({"call_id": call.call_id, "tool_name": call.tool_name});
    format!(
        "[Runtime tool error: ToolExecutionOutcomeUnknown]\n{identity}\n{OUTCOME_UNKNOWN_NOTICE}"
    )
}

fn has_outcome_notice(messages: &[Message], call: &InterruptedToolCall) -> bool {
    let notice = outcome_notice(call);
    messages.iter().any(|message| {
        message.role == Role::User
            && message
                .content
                .iter()
                .any(|block| matches!(block, ContentBlock::Text { text } if text == &notice))
    })
}

impl RunRestoreDescriptor {
    /// Records written before outcome recovery gave uncertainty precedence over
    /// authority-renewal reasons. Only current-authority, root chat restoration
    /// may reinterpret that marker. It must never revive disk execution authority
    /// or bypass consumed/unsupported records. Team identity still needs renewal.
    pub(crate) fn conversation_descriptor(&self) -> Self {
        let mut descriptor = self.clone();
        if !self.interrupted_tool_calls.is_empty()
            && self.parent_run_id.is_none()
            && self.non_restorable_reason.as_deref() == Some(UNRESOLVED_TOOL_CALLS_REASON)
        {
            descriptor.restorable = false;
            descriptor.non_restorable_reason = Some(
                if self.renewable_team.is_some() {
                    super::TEAM_RENEWAL_REQUIRED_REASON
                } else {
                    super::ROOT_CONTEXT_RENEWAL_REQUIRED_REASON
                }
                .into(),
            );
        }
        descriptor.interrupted_tool_calls.clear();
        descriptor
    }
}
