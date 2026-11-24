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

impl super::Shared {
    /// Whether the file `id` of `thread` may be removed now: not while the thread's
    /// message is being sent, nor while the file's upload awaits its reply.
    pub fn can_remove_attachment(&self, thread: &str, id: &str) -> bool {
        !self.sending.contains(thread)
            && self
                .attachment_drafts
                .get(thread)
                .and_then(|drafts| drafts.iter().find(|draft| draft.id == id))
                .is_some_and(|draft| !draft.uploading)
    }

    /// Removes the file `id` from `thread`'s pending attachments. `None` when it is
    /// refused (see [`Self::can_remove_attachment`]) or gone; otherwise the core's
    /// upload of it, if one began, which is the caller's to discard.
    pub fn remove_attachment_draft(
        &mut self,
        thread: &str,
        id: &str,
    ) -> Option<Option<AttachmentId>> {
        if !self.can_remove_attachment(thread, id) {
            return None;
        }
        let drafts = self.attachment_drafts.get_mut(thread)?;
        let position = drafts.iter().position(|draft| draft.id == id)?;
        let removed = drafts.remove(position);
        if drafts.is_empty() {
            self.attachment_drafts.remove(thread);
        }
        Some(removed.upload_id)
    }
}

#[cfg(test)]
mod tests {
    use super::super::Shared;
    use super::*;

    fn draft(id: &str, uploading: bool, upload_id: Option<&str>) -> ClientAttachmentDraft {
        let mut draft = ClientAttachmentDraft::new(
            ProjectId::new("p").unwrap(),
            AttachmentFile {
                draft_id: id.into(),
                filename: format!("{id}.txt"),
                media_type: "text/plain".into(),
                bytes: b"hello".to_vec(),
            },
        );
        draft.uploading = uploading;
        draft.upload_id = upload_id.map(|id| AttachmentId::new(id).unwrap());
        draft
    }

    // Given: a thread with an uploaded file, a file whose upload awaits its reply and a
    //        file whose upload never began
    // When: each is removed, and then the thread's message is being sent
    // Then: the uploading one is refused, the others go and hand back the core's upload
    //       to discard; nothing is removed while sending
    #[test]
    fn remove_refuses_an_uploading_file_and_a_sending_thread() {
        let mut shared = Shared::default();
        shared.attachment_drafts.insert(
            "t".into(),
            vec![
                draft("ready", false, Some("up-1")),
                draft("busy", true, Some("up-2")),
                draft("local", false, None),
            ],
        );
        assert_eq!(shared.remove_attachment_draft("t", "busy"), None);
        assert_eq!(
            shared.remove_attachment_draft("t", "ready"),
            Some(Some(AttachmentId::new("up-1").unwrap()))
        );
        shared.sending.insert("t".into());
        assert_eq!(shared.remove_attachment_draft("t", "local"), None);
        shared.sending.clear();
        assert_eq!(shared.remove_attachment_draft("t", "local"), Some(None));
        assert_eq!(shared.remove_attachment_draft("t", "gone"), None);
        let left: Vec<_> = shared.attachment_drafts["t"]
            .iter()
            .map(|draft| draft.id.as_str())
            .collect();
        assert_eq!(left, ["busy"]);
    }
}
