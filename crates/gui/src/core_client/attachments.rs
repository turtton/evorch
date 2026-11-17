//! Upload progression is driven only by acknowledged command replies (including
//! receipt lookup). An unknown outcome cannot allocate or append another upload.
use base64::{Engine, engine::general_purpose::STANDARD};
use sha2::{Digest, Sha256};
use workbench_protocol::{
    commands::{AttachmentRef, Command},
    conversation::{ATTACHMENT_CHUNK_BYTES, ConversationCommand, ConversationResult},
    ids::{AttachmentId, ProjectId, ThreadId},
};

use crate::attachments::AttachmentFile;

#[derive(Clone)]
pub struct ClientAttachmentDraft {
    pub id: String,
    pub project: ProjectId,
    pub file: AttachmentFile,
    pub uploading: bool,
    pub uncertain: bool,
    pub upload_id: Option<AttachmentId>,
    pub next_offset: u64,
    pub reference: Option<AttachmentRef>,
}

#[derive(Clone)]
pub(super) enum ExpectedUploadReply {
    Begin,
    Append(u64),
    Finish,
}

impl ClientAttachmentDraft {
    pub(super) fn new(project: ProjectId, file: AttachmentFile) -> Self {
        Self {
            id: file.draft_id.clone(),
            project,
            file,
            uploading: false,
            uncertain: false,
            upload_id: None,
            next_offset: 0,
            reference: None,
        }
    }

    pub(super) fn next_command(&self, thread: &str) -> Option<(Command, ExpectedUploadReply)> {
        if !self.uploading || self.uncertain || self.reference.is_some() {
            return None;
        }
        let (command, expected) = match &self.upload_id {
            None => (
                ConversationCommand::BeginAttachment {
                    project: self.project.clone(),
                    thread: ThreadId::new(thread).ok()?,
                    client_upload_id: self.id.clone(),
                    filename: self.file.filename.clone(),
                    media_type_hint: self.file.media_type.clone(),
                    byte_length: self.file.bytes.len() as u64,
                },
                ExpectedUploadReply::Begin,
            ),
            Some(upload_id) if self.next_offset < self.file.bytes.len() as u64 => {
                let start = usize::try_from(self.next_offset).ok()?;
                let end = (start + ATTACHMENT_CHUNK_BYTES).min(self.file.bytes.len());
                (
                    ConversationCommand::AppendAttachment {
                        upload_id: upload_id.clone(),
                        offset: self.next_offset,
                        data_base64: STANDARD.encode(&self.file.bytes[start..end]),
                    },
                    ExpectedUploadReply::Append(end as u64),
                )
            }
            Some(upload_id) => (
                ConversationCommand::FinishAttachment {
                    upload_id: upload_id.clone(),
                },
                ExpectedUploadReply::Finish,
            ),
        };
        Some((Command::Conversation(command), expected))
    }

    pub(super) fn acknowledge(
        &mut self,
        expected: ExpectedUploadReply,
        result: &ConversationResult,
    ) -> Result<(), String> {
        match (expected, result) {
            (
                ExpectedUploadReply::Begin,
                ConversationResult::AttachmentBegun { upload_id, limits },
            ) if limits.file_bytes >= self.file.bytes.len() as u64
                && limits.chunk_bytes >= ATTACHMENT_CHUNK_BYTES as u64 =>
            {
                self.upload_id = Some(upload_id.clone());
                self.next_offset = 0;
            }
            (
                ExpectedUploadReply::Append(expected),
                ConversationResult::AttachmentAppended { next_offset },
            ) if expected == *next_offset => self.next_offset = *next_offset,
            (ExpectedUploadReply::Finish, ConversationResult::AttachmentFinished(reference))
                if self.upload_id.as_ref() == Some(&reference.id)
                    && reference.filename == self.file.filename
                    && reference.byte_length == self.file.bytes.len() as u64
                    && reference.sha256 == format!("{:x}", Sha256::digest(&self.file.bytes)) =>
            {
                self.reference = Some(reference.clone());
                self.uploading = false;
            }
            _ => {
                return Err("Attachment reply does not match the acknowledged upload stage".into());
            }
        }
        self.uncertain = false;
        Ok(())
    }
}
