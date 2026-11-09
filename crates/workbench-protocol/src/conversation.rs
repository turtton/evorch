//! Conversation bootstrap shared by Desktop, Web and CLI. No client paths are
//! accepted for attachments. A project root is only a lookup in host declarations,
//! not a trust declaration or a request to add project settings.
use crate::{
    commands::{ImageRef, SendChat},
    ids::{AttachmentId, ProjectId, ThreadId},
    validation::{ValidationError, check_line, check_text},
};
use serde::{Deserialize, Serialize};

/// Base64 bound keeps a complete upload below the default 1 MiB frame.
pub const MAX_IMAGE_BASE64_BYTES: usize = 512 * 1024;
pub const MAX_PROJECT_ROOT_BYTES: usize = 4096;

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
}
impl ConversationCommand {
    pub fn validate(&self) -> Result<(), ValidationError> {
        match self {
            Self::ResolveProject { root } => {
                check_line("root", root, MAX_PROJECT_ROOT_BYTES, false)
            }
            Self::CreateExecution(chat) => chat.validate(),
            Self::UploadImage {
                mime_type, data, ..
            } => {
                check_line("mime_type", mime_type, crate::limits::MAX_MIME_BYTES, false)?;
                check_text("data", data, MAX_IMAGE_BASE64_BYTES, false)
            }
            Self::PrepareExistingThread { .. } | Self::DiscardAttachment { .. } => Ok(()),
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
pub enum ConversationResult {
    ProjectResolved { project: ProjectId },
    ExistingThreadPrepared { thread: ThreadId },
    ImageUploaded(ImageRef),
    AttachmentDiscarded,
}
