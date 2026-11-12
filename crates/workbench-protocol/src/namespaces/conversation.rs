//! Conversation namespace, shared by Desktop, Web and CLI: creating, sending
//! to and reading a conversation, its attachments and its input receipts.
//!
//! No client paths are accepted for attachments. A project root is only a
//! lookup in host declarations, not a trust declaration or a request to add
//! project settings.
use crate::{
    commands::{
        ContinueChat, ImageRef, ModelPreference, SendChat, SubmitGoal, ThreadTarget,
        WithdrawFollowUp,
    },
    handshake::{Capability, WireLimits},
    ids::{AttachmentId, ProjectId, ThreadId},
    queries::{ReadCommittedHistory, ThreadQuery},
    validation::{ValidationError, check_line, check_text},
    views::{CommittedMessage, InputReceipt, Page},
};
use serde::{Deserialize, Serialize};

/// Base64 bound keeps a complete upload below the default 1 MiB frame.
pub const MAX_IMAGE_BASE64_BYTES: usize = 512 * 1024;
pub const MAX_PROJECT_ROOT_BYTES: usize = 4096;

/// Wire names of the namespace's commands.
pub const COMMAND_OPERATIONS: &[&str] = &[
    "resolve_project",
    "create_execution",
    "create_goal",
    "prepare_existing_thread",
    "upload_image",
    "discard_attachment",
    "send_chat",
    "continue_chat",
    "compact_chat",
    "deliver_follow_ups_next_turn",
    "withdraw_follow_up",
];
/// Wire names of the namespace's queries.
pub const QUERY_OPERATIONS: &[&str] = &["get_input_receipts", "read_committed_history"];
/// Wire names of the namespace's events.
pub const EVENT_TYPES: &[&str] = &["input_receipt_changed", "message_committed"];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ConversationCommand {
    ResolveProject {
        root: String,
    },
    /// Client-generated ids make the first human input correlatable by receipt.
    CreateExecution(SendChat),
    /// Start the desktop team/goal entry point without a synthetic chat prompt.
    CreateGoal(SubmitGoal),
    /// Opt-in legacy import. Does not itself start/resume an agent. Only a
    /// released, demonstrably offline owner may be claimed; never force a lease.
    PrepareExistingThread {
        project: ProjectId,
        thread: ThreadId,
    },
    /// Bytes, never a local path. The returned id is scoped to project + thread
    /// and this core epoch. Clients retain unsent bytes across epoch changes.
    UploadImage {
        project: ProjectId,
        thread: ThreadId,
        mime_type: String,
        data: String,
    },
    DiscardAttachment {
        attachment_id: AttachmentId,
    },
    /// Send a user message to a thread.
    SendChat(SendChat),
    /// Ask the core to continue a thread. Automatic continuations are the
    /// core's own decision and cannot be requested by a client.
    ContinueChat(ContinueChat),
    CompactChat(ThreadTarget),
    DeliverFollowUpsNextTurn(ThreadTarget),
    /// Take back a queued follow-up that the agent has not adopted yet.
    WithdrawFollowUp(WithdrawFollowUp),
}

impl ConversationCommand {
    /// The capability a core must offer before this command may be sent.
    pub fn required_capability(&self) -> Capability {
        match self {
            Self::ResolveProject { .. }
            | Self::CreateExecution(_)
            | Self::CreateGoal(_)
            | Self::PrepareExistingThread { .. }
            | Self::UploadImage { .. }
            | Self::DiscardAttachment { .. } => Capability::Conversation,
            Self::SendChat(_) | Self::WithdrawFollowUp(_) => Capability::InputReceipts,
            Self::ContinueChat(_) | Self::CompactChat(_) | Self::DeliverFollowUpsNextTurn(_) => {
                Capability::Commands
            }
        }
    }

    pub fn validate(&self) -> Result<(), ValidationError> {
        match self {
            Self::ResolveProject { root } => {
                check_line("root", root, MAX_PROJECT_ROOT_BYTES, false)
            }
            Self::CreateExecution(chat) | Self::SendChat(chat) => chat.validate(),
            Self::CreateGoal(goal) => goal.validate(),
            Self::ContinueChat(chat) => chat
                .model_preference
                .as_ref()
                .map_or(Ok(()), ModelPreference::validate),
            Self::UploadImage {
                mime_type, data, ..
            } => {
                check_line("mime_type", mime_type, crate::limits::MAX_MIME_BYTES, false)?;
                check_text("data", data, MAX_IMAGE_BASE64_BYTES, false)
            }
            Self::PrepareExistingThread { .. }
            | Self::DiscardAttachment { .. }
            | Self::CompactChat(_)
            | Self::DeliverFollowUpsNextTurn(_)
            | Self::WithdrawFollowUp(_) => Ok(()),
        }
    }
}

/// What a conversation command answers with, beyond the common receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ConversationResult {
    ProjectResolved { project: ProjectId },
    ExistingThreadPrepared { thread: ThreadId },
    ImageUploaded(ImageRef),
    AttachmentDiscarded,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ConversationQuery {
    GetInputReceipts(ThreadQuery),
    ReadCommittedHistory(ReadCommittedHistory),
}

impl ConversationQuery {
    pub fn required_capability(&self) -> Capability {
        match self {
            Self::GetInputReceipts(_) => Capability::InputReceipts,
            Self::ReadCommittedHistory(_) => Capability::CommittedBodies,
        }
    }

    pub fn validate(&self, _limits: &WireLimits) -> Result<(), ValidationError> {
        match self {
            Self::GetInputReceipts(q) => q.page.validate(),
            Self::ReadCommittedHistory(q) => q.page.validate(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ConversationReply {
    InputReceipts(Page<InputReceipt>),
    CommittedHistory(Page<CommittedMessage>),
}

impl ConversationReply {
    pub fn validate(&self, _limits: &WireLimits) -> Result<(), ValidationError> {
        match self {
            Self::InputReceipts(page) => page.validate_with(InputReceipt::validate),
            Self::CommittedHistory(page) => page.validate_with(CommittedMessage::validate),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ConversationEvent {
    InputReceiptChanged { receipt: InputReceipt },
    MessageCommitted { message: CommittedMessage },
}

impl ConversationEvent {
    pub fn validate(&self) -> Result<(), ValidationError> {
        match self {
            Self::InputReceiptChanged { receipt } => receipt.validate(),
            Self::MessageCommitted { message } => message.validate(),
        }
    }
}
