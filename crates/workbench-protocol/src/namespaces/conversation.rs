//! Conversation namespace, shared by Desktop, Web and CLI: creating, sending
//! to and reading a conversation, its attachments and its input receipts.
//!
//! No client paths are accepted for attachments. A project root is only a
//! lookup in host declarations, not a trust declaration or a request to add
//! project settings.
use crate::{
    commands::{AttachmentRef, ContinueChat, SendChat, SubmitGoal, ThreadTarget, WithdrawFollowUp},
    handshake::{Capability, WireLimits},
    ids::{AttachmentId, ProjectId, ThreadId},
    queries::{ReadCommittedHistory, ThreadQuery},
    validation::{ValidationError, check_line, check_text},
    views::{CommittedMessage, InputReceipt, Page},
};
use serde::{Deserialize, Serialize};

pub mod input_draft;
pub use input_draft::{GetInputDraft, InputDraft, MAX_INPUT_DRAFT_BYTES};

/// Base64 bound keeps a complete upload below the default 1 MiB frame.
pub const ATTACHMENT_FILE_BYTES: u64 = 8 * 1024 * 1024;
pub const ATTACHMENT_CHUNK_BYTES: usize = 256 * 1024;
pub const MAX_ATTACHMENT_CHUNK_BASE64_BYTES: usize = (256 * 1024_usize).div_ceil(3) * 4;
pub const MAX_PROJECT_ROOT_BYTES: usize = 4096;

/// Wire names of the namespace's commands.
pub const COMMAND_OPERATIONS: &[&str] = &[
    "resolve_project",
    "create_execution",
    "create_goal",
    "begin_attachment",
    "append_attachment",
    "finish_attachment",
    "discard_attachment",
    "send_chat",
    "continue_chat",
    "compact_chat",
    "deliver_follow_ups_next_turn",
    "withdraw_follow_up",
];
/// Wire names of the namespace's queries.
pub const QUERY_OPERATIONS: &[&str] = &[
    "get_input_receipts",
    "get_input_draft",
    "read_committed_history",
];
/// Wire names of the namespace's events.
pub const EVENT_TYPES: &[&str] = &["input_receipt_changed", "message_committed"];
/// Tagged choices inside a command's payload that classification names (none).
pub const NESTED_COMMANDS: &[super::NestedOperation] = &[];

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
    BeginAttachment {
        project: ProjectId,
        thread: ThreadId,
        client_upload_id: String,
        filename: String,
        media_type_hint: String,
        byte_length: u64,
    },
    AppendAttachment {
        upload_id: AttachmentId,
        offset: u64,
        data_base64: String,
    },
    FinishAttachment {
        upload_id: AttachmentId,
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
            | Self::BeginAttachment { .. }
            | Self::AppendAttachment { .. }
            | Self::FinishAttachment { .. }
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
            Self::ContinueChat(chat) => {
                if chat.model_preference.is_some() {
                    Err(ValidationError::OutOfRange {
                        field: "model_preference",
                        reason: "ContinueChat cannot change selection; use SetThreadModelChoice",
                    })
                } else {
                    Ok(())
                }
            }
            Self::BeginAttachment {
                client_upload_id,
                filename,
                media_type_hint,
                byte_length,
                ..
            } => {
                check_line("client_upload_id", client_upload_id, 128, false)?;
                check_line("filename", filename, 255, false)?;
                check_line(
                    "media_type_hint",
                    media_type_hint,
                    crate::limits::MAX_MIME_BYTES,
                    true,
                )?;
                if *byte_length > 8 * 1024 * 1024 {
                    return Err(ValidationError::OutOfRange {
                        field: "byte_length",
                        reason: "attachment exceeds 8 MiB",
                    });
                }
                Ok(())
            }
            Self::AppendAttachment { data_base64, .. } => check_text(
                "data_base64",
                data_base64,
                MAX_ATTACHMENT_CHUNK_BASE64_BYTES,
                true,
            ),
            Self::FinishAttachment { .. } => Ok(()),
            Self::DiscardAttachment { .. }
            | Self::CompactChat(_)
            | Self::DeliverFollowUpsNextTurn(_)
            | Self::WithdrawFollowUp(_) => Ok(()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttachmentLimits {
    pub file_bytes: u64,
    pub chunk_bytes: u64,
    pub thread_pending_count: u32,
    pub thread_pending_bytes: u64,
    pub store_pending_count: u32,
    pub store_pending_bytes: u64,
    pub thread_sent_bytes: u64,
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
    ProjectResolved {
        project: ProjectId,
    },
    AttachmentBegun {
        upload_id: AttachmentId,
        limits: AttachmentLimits,
    },
    AttachmentAppended {
        next_offset: u64,
    },
    AttachmentFinished(AttachmentRef),
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
    /// The words of one input a person can edit or restore, read on demand.
    GetInputDraft(GetInputDraft),
    ReadCommittedHistory(ReadCommittedHistory),
}

impl ConversationQuery {
    pub fn required_capability(&self) -> Capability {
        match self {
            Self::GetInputReceipts(_) | Self::GetInputDraft(_) => Capability::InputReceipts,
            Self::ReadCommittedHistory(_) => Capability::CommittedBodies,
        }
    }

    pub fn validate(&self, _limits: &WireLimits) -> Result<(), ValidationError> {
        match self {
            Self::GetInputReceipts(q) => q.page.validate(),
            Self::GetInputDraft(_) => Ok(()),
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
    InputDraft(InputDraft),
    CommittedHistory(Page<CommittedMessage>),
}

impl ConversationReply {
    pub fn validate(&self, _limits: &WireLimits) -> Result<(), ValidationError> {
        match self {
            Self::InputReceipts(page) => page.validate_with(InputReceipt::validate),
            Self::InputDraft(draft) => draft.validate(),
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
