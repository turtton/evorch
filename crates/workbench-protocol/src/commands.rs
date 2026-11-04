//! State-changing requests.
//!
//! Types and validation only; the host handlers that execute them come later.
//! Meanings follow the existing in-process commands (`WorkbenchCommand` in the
//! GUI, `ModelPreference` and `ChatForkSeed` in the runtime) without depending
//! on them. Every struct rejects unknown fields, so a client cannot smuggle in
//! host-only inputs such as an `automatic` continuation flag or a provenance.

use crate::handshake::Capability;
use crate::ids::{
    ApprovalId, AttachmentId, GoalId, ProjectId, QuestionId, RunId, SubmissionId, ThreadId,
};
use crate::limits::{
    MAX_CONSTRAINT_BYTES, MAX_CONSTRAINTS, MAX_IMAGES, MAX_MIME_BYTES, MAX_NAME_BYTES,
    MAX_OBJECTIVE_BYTES, MAX_REFERENCE_BYTES, MAX_REFERENCES, MAX_SKILLS, MAX_TEXT_BYTES,
};
use crate::validation::{ValidationError, check_all, check_count, check_line, check_text};
use crate::wire::{InstanceEpoch, MessageCount, Revision};
use serde::{Deserialize, Serialize};

/// Which composer a message was typed into.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComposerRole {
    #[default]
    Worker,
    Orchestrator,
}

/// An uploaded image a message refers to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageRef {
    pub attachment_id: AttachmentId,
    pub mime_type: String,
}

/// Explicit provider selection for one chat; `model: None` uses the profile default.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelPreference {
    pub profile: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
}

/// Completed-turn boundary a forked thread starts from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForkSeed {
    pub source_run_id: RunId,
    /// Non-system message count published by the turn the fork starts after.
    pub context_len: MessageCount,
}

/// Send a user message to a thread.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SendChat {
    pub project: ProjectId,
    pub thread: ThreadId,
    /// Names this message across its delivery states; resending the same id is a no-op.
    pub submission_id: SubmissionId,
    #[serde(default)]
    pub composer_role: ComposerRole,
    /// What the user typed; skills are expanded by the core, not the client.
    pub text: String,
    #[serde(default)]
    pub skills: Vec<String>,
    #[serde(default)]
    pub images: Vec<ImageRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_preference: Option<ModelPreference>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fork_seed: Option<ForkSeed>,
}

/// Ask the core to continue a thread. Automatic continuations are the core's
/// own decision and cannot be requested by a client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContinueChat {
    pub project: ProjectId,
    pub thread: ThreadId,
    #[serde(default)]
    pub composer_role: ComposerRole,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_preference: Option<ModelPreference>,
}

/// A command that only names a thread.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThreadTarget {
    pub thread: ThreadId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StopRun {
    pub thread: ThreadId,
    pub run: RunId,
}

/// Take back a queued follow-up that the agent has not adopted yet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WithdrawFollowUp {
    pub thread: ThreadId,
    pub run: RunId,
    pub submission_id: SubmissionId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReferenceKind {
    Packet,
    Issue,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PacketReference {
    pub kind: ReferenceKind,
    pub value: String,
}

/// How a goal is handed to agents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum Delegation {
    /// The thread's own orchestrator works the goal.
    Direct,
    /// Hand the goal to a packet or issue the host resolves.
    Reference { value: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubmitGoal {
    pub project: ProjectId,
    pub thread: ThreadId,
    pub objective: String,
    #[serde(default)]
    pub references: Vec<PacketReference>,
    #[serde(default)]
    pub constraints: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delegation: Option<Delegation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetGoalReview {
    pub thread: ThreadId,
    pub goal: GoalId,
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetGoalChecksPaused {
    pub thread: ThreadId,
    pub goal: GoalId,
    pub paused: bool,
}

/// Answer a question an agent asked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnswerUserQuestion {
    pub thread: ThreadId,
    pub question_id: QuestionId,
    pub answer: String,
    /// The question revision the user saw; a changed question is refused as stale.
    pub question_revision: Revision,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    Approve,
    Deny,
}

/// Approve or deny one tool call. The epoch and revision stop a decision made
/// against a request that was replaced or lost in a core restart.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecideToolApproval {
    pub approval_id: ApprovalId,
    pub core_epoch: InstanceEpoch,
    pub approval_revision: Revision,
    pub decision: ApprovalDecision,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetAgentWebToolsEnabled {
    pub enabled: bool,
}

/// Every state-changing request a client may send.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum Command {
    SendChat(SendChat),
    ContinueChat(ContinueChat),
    CompactChat(ThreadTarget),
    StopChat(ThreadTarget),
    StopRun(StopRun),
    DeliverFollowUpsNextTurn(ThreadTarget),
    WithdrawFollowUp(WithdrawFollowUp),
    SubmitGoal(SubmitGoal),
    SetGoalReview(SetGoalReview),
    SetGoalChecksPaused(SetGoalChecksPaused),
    AnswerUserQuestion(AnswerUserQuestion),
    DecideToolApproval(DecideToolApproval),
    SetAgentWebToolsEnabled(SetAgentWebToolsEnabled),
}

impl Command {
    /// Wire names of every variant, for rejecting unknown ones precisely.
    pub const TYPES: [&'static str; 13] = [
        "send_chat",
        "continue_chat",
        "compact_chat",
        "stop_chat",
        "stop_run",
        "deliver_follow_ups_next_turn",
        "withdraw_follow_up",
        "submit_goal",
        "set_goal_review",
        "set_goal_checks_paused",
        "answer_user_question",
        "decide_tool_approval",
        "set_agent_web_tools_enabled",
    ];

    /// The capability a core must offer before this command may be sent.
    pub fn required_capability(&self) -> Capability {
        match self {
            Self::AnswerUserQuestion(_) => Capability::Questions,
            Self::DecideToolApproval(_) => Capability::Approvals,
            Self::SendChat(_) | Self::WithdrawFollowUp(_) => Capability::InputReceipts,
            _ => Capability::Commands,
        }
    }

    /// Check every string, collection and cross-field rule.
    pub fn validate(&self) -> Result<(), ValidationError> {
        match self {
            Self::SendChat(c) => c.validate(),
            Self::ContinueChat(c) => c
                .model_preference
                .as_ref()
                .map_or(Ok(()), ModelPreference::validate),
            Self::SubmitGoal(c) => c.validate(),
            Self::AnswerUserQuestion(c) => check_text("answer", &c.answer, MAX_TEXT_BYTES, false),
            Self::CompactChat(_)
            | Self::StopChat(_)
            | Self::StopRun(_)
            | Self::DeliverFollowUpsNextTurn(_)
            | Self::WithdrawFollowUp(_)
            | Self::SetGoalReview(_)
            | Self::SetGoalChecksPaused(_)
            | Self::DecideToolApproval(_)
            | Self::SetAgentWebToolsEnabled(_) => Ok(()),
        }
    }
}

impl ModelPreference {
    pub fn validate(&self) -> Result<(), ValidationError> {
        check_line(
            "model_preference.profile",
            &self.profile,
            MAX_NAME_BYTES,
            false,
        )?;
        if let Some(model) = &self.model {
            check_line("model_preference.model", model, MAX_NAME_BYTES, false)?;
        }
        if let Some(effort) = &self.reasoning_effort {
            check_line(
                "model_preference.reasoning_effort",
                effort,
                MAX_NAME_BYTES,
                false,
            )?;
        }
        Ok(())
    }
}

impl SendChat {
    pub fn validate(&self) -> Result<(), ValidationError> {
        // A message may be only images, so text can be empty then.
        check_text("text", &self.text, MAX_TEXT_BYTES, !self.images.is_empty())?;
        check_count("skills", self.skills.len(), MAX_SKILLS)?;
        check_all(&self.skills, |s| {
            check_line("skills", s, MAX_NAME_BYTES, false)
        })?;
        check_count("images", self.images.len(), MAX_IMAGES)?;
        check_all(&self.images, |i| {
            check_line("images.mime_type", &i.mime_type, MAX_MIME_BYTES, false)?;
            if !i.mime_type.starts_with("image/") {
                return Err(ValidationError::OutOfRange {
                    field: "images.mime_type",
                    reason: "must be an image/* media type",
                });
            }
            Ok(())
        })?;
        self.model_preference
            .as_ref()
            .map_or(Ok(()), ModelPreference::validate)
    }
}

impl SubmitGoal {
    pub fn validate(&self) -> Result<(), ValidationError> {
        check_text("objective", &self.objective, MAX_OBJECTIVE_BYTES, false)?;
        check_count("references", self.references.len(), MAX_REFERENCES)?;
        check_all(&self.references, |r| {
            check_line("references.value", &r.value, MAX_REFERENCE_BYTES, false)
        })?;
        check_count("constraints", self.constraints.len(), MAX_CONSTRAINTS)?;
        check_all(&self.constraints, |c| {
            check_text("constraints", c, MAX_CONSTRAINT_BYTES, false)
        })?;
        if let Some(Delegation::Reference { value }) = &self.delegation {
            check_line("delegation.value", value, MAX_REFERENCE_BYTES, false)?;
        }
        Ok(())
    }
}

/// How the core took a command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandDisposition {
    Accepted,
    /// The same request or submission was applied before; nothing changed now.
    AlreadyApplied,
}

/// Success answer to a command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandReply {
    pub disposition: CommandDisposition,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub submission_id: Option<SubmissionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<RunId>,
    /// Revision of the thread after the command, when it changed one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_revision: Option<Revision>,
}
