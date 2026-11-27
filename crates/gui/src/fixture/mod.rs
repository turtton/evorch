//! Deterministic fixtures for tests of the shared UI parts.

mod codex_auth;
mod conversation_plan;

pub use codex_auth::ScriptedCodexAuthBackend;
pub use conversation_plan::{FixturePlanReader, PlanScenario};

/// A metadata-only, acknowledged attachment for composer fixtures.
pub fn attachment_draft() -> crate::model::composer::AttachmentDraft {
    use crate::model::composer::{AttachmentDraft, AttachmentUploadState};
    let file = crate::attachments::AttachmentFile {
        draft_id: uuid::Uuid::new_v4().to_string(),
        filename: "notes.txt".into(),
        media_type: "text/plain".into(),
        bytes: b"hello".to_vec(),
    };
    let mut draft = AttachmentDraft::new(file);
    draft.upload = AttachmentUploadState::Ready(workbench_protocol::commands::AttachmentRef {
        id: workbench_protocol::ids::AttachmentId::new("fixture-attachment").unwrap(),
        filename: draft.file.filename.clone(),
        media_type: draft.file.media_type.clone(),
        byte_length: draft.file.bytes.len() as u64,
        sha256: "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824".into(),
    });
    draft
}
