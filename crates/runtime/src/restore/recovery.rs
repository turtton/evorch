//! Conversation recovery is not tool replay. Keep the saved provider prefix intact
//! and append an outcome-unknown error for calls whose results could not be saved.
use super::{InterruptedToolCall, RestoredState, RunRestoreDescriptor};
use providers::{ContentBlock, Message, Role};
use storage::RunContextRecord;
use storage::thread_index::SnapshotBundle;

pub(super) const UNRESOLVED_TOOL_CALLS_REASON: &str =
    "unresolved_tool_calls: inspect actual effects before starting a new run";

const OUTCOME_UNKNOWN_NOTICE: &str = "The execution result was not retained in the recoverable history, or execution was interrupted. \
    The operation may have partially or fully executed; neither success nor absence of effects is established. \
    Conversation history has been retained. The runtime has not replayed this operation or resumed its old shell jobs. \
    Continue responding to the user. If further work depends on this outcome, inspect the current state before deciding whether to retry; do not blindly repeat the operation.";

impl RestoredState {
    /// Only an explicit human Continue may abandon validated waits. Append
    /// results after the immutable sent prefix, without observing child output,
    /// consuming mailboxes, or redispatching any original tool.
    pub(crate) fn for_human_recovery_bundle(
        bundle: &SnapshotBundle,
    ) -> Result<Self, crate::RuntimeError> {
        let mut descriptor: RunRestoreDescriptor = serde_json::from_str(&bundle.record.config_json)
            .map_err(|e| crate::quiesce::runtime::error(e.to_string()))?;
        if descriptor.recovery_waits.is_empty() {
            return Self::for_conversation_bundle(bundle);
        }
        super::validate_recovery_waits(&bundle.record, &descriptor.recovery_waits)?;
        let mut messages: Vec<Message> = serde_json::from_str(&bundle.record.messages_json)
            .map_err(|e| crate::quiesce::runtime::error(e.to_string()))?;
        let saved_len = messages.len();
        for wait in &descriptor.recovery_waits {
            messages.push(Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_call_id: wait.tool_call_id.clone(),
                    content: vec![providers::ToolResultContent::Text {
                        text: "Wait aborted by explicit human Continue. The tool was not replayed and child output was not observed; unread messages and notices remain unread.".into(),
                    }],
                    is_error: true,
                }],
            });
        }
        descriptor.recovery_waits.clear();
        descriptor.recovery_boundary = None;
        let mut record = bundle.record.clone();
        record.messages_json =
            serde_json::to_string(&messages).expect("validated messages serialize");
        record.config_json =
            serde_json::to_string(&descriptor).expect("validated descriptor serializes");
        let mut restored = Self::for_conversation(&record)?;
        restored.attach_bundle(bundle);
        restored
            .provenance
            .generated_since(&restored.messages, saved_len);
        Ok(restored)
    }

    /// The bundle's body, provenance and revision all come from one read, so a
    /// later save is conditioned on exactly the snapshot this state was built from.
    pub(crate) fn from_bundle(bundle: &SnapshotBundle) -> Result<Self, crate::RuntimeError> {
        Self::from_recovery_bundle(bundle, &[])
    }

    /// [`Self::for_conversation`] over one bundle read; see [`Self::from_bundle`].
    pub(crate) fn for_conversation_bundle(
        bundle: &SnapshotBundle,
    ) -> Result<Self, crate::RuntimeError> {
        let mut restored = Self::for_conversation(&bundle.record)?;
        restored.attach_bundle(bundle);
        let saved_messages: Vec<serde_json::Value> =
            serde_json::from_str(&bundle.record.messages_json)
                .expect("conversation restore validated JSON");
        restored
            .provenance
            .generated_since(&restored.messages, saved_messages.len());
        Ok(restored)
    }

    fn attach_bundle(&mut self, bundle: &SnapshotBundle) {
        self.provenance = super::provenance::ConversationProvenance::from_bundle(bundle);
        self.snapshot_revision = super::revision::SnapshotRevision::known(bundle.snapshot_revision);
    }

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
            if !descriptor.outcome_notices_persisted
                && (call.call_id == "unobserved-shell-jobs"
                    || !has_outcome_notice(&restored.messages, call))
            {
                restored.turn_completed = false;
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
