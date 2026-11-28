//! Editing data of queued human input (ADR 0045 E7): what is kept when a chat is
//! accepted, the preview a receipt carries, and the words read by id.
//!
//! Lists read only previews of the inputs a person can still act on. The words are read for
//! one input at a time, through a storage read of that single id; the receipt projection
//! (`collect`) never selects them.
use super::{error, masked, storage_error, text};
use crate::commands::ChatInput;
use runtime::AgentRuntime;
use workbench_protocol::{
    errors::{ErrorCode, WorkbenchError},
    ids::RunId,
    namespaces::conversation::{GetInputDraft, InputDraft, MAX_INPUT_DRAFT_BYTES},
    views::{InputPreview, InputReceiptState, MAX_INPUT_PREVIEW_BYTES},
};

/// The edit data to store with an accepted chat: its words as typed, before any attachment
/// path is added, and how many attachments (images among them) it carried. Dropped when it
/// cannot be stored; the chat is accepted either way and merely cannot be edited later.
pub(crate) fn edit_data(chat: &ChatInput) -> Option<storage::HostChatSubmissionEdit> {
    let attachments = chat.attachments.len();
    let images = chat
        .attachments
        .iter()
        .filter(|a| a.immutable_metadata().1.starts_with("image/"))
        .count();
    let edit = storage::HostChatSubmissionEdit::new(
        &chat.text,
        u32::try_from(images).ok()?,
        u32::try_from(attachments).ok()?,
    )?;
    match edit.validate() {
        Ok(()) => Some(edit),
        Err(error) => {
            tracing::warn!(%error, submission = %chat.submission_id, "chat cannot be edited later");
            None
        }
    }
}

/// Whether a receipt in `status` shows a preview and may be edited or restored.
pub(crate) fn is_queued(status: storage::HostInputStatus) -> bool {
    matches!(
        status,
        storage::HostInputStatus::Accepted
            | storage::HostInputStatus::Dropped
            | storage::HostInputStatus::Unknown
    )
}

/// The wire preview of stored data. The words are masked again here: a secret that
/// became known after they were stored must not reach a client.
pub(crate) fn preview_view(stored: &storage::HostInputPreview) -> InputPreview {
    let shown = text(&stored.preview, MAX_INPUT_PREVIEW_BYTES);
    InputPreview {
        truncated: stored.preview_truncated || shown.len() < masked(&stored.preview).len(),
        text: shown,
        image_count: stored.image_count,
        attachment_count: stored.attachment_count,
    }
}

fn state_of(status: storage::HostInputStatus) -> InputReceiptState {
    match status {
        storage::HostInputStatus::Accepted => InputReceiptState::Accepted,
        storage::HostInputStatus::Withdrawn => InputReceiptState::Withdrawn,
        storage::HostInputStatus::Delivered => InputReceiptState::Delivered,
        storage::HostInputStatus::Dropped => InputReceiptState::Dropped,
        storage::HostInputStatus::Unknown => InputReceiptState::Unknown,
    }
}

fn not_found() -> WorkbenchError {
    // The same answer for a missing, foreign or deleted input: nothing says which.
    error(ErrorCode::NotFound, "no editable input")
}

/// The words of one input of `query.thread`, for edit or restore. Answers only an
/// `Accepted`, `Dropped` or `Unknown` input; never cuts the words.
pub(crate) fn read(
    runtime: &AgentRuntime,
    query: &GetInputDraft,
) -> Result<InputDraft, WorkbenchError> {
    let read = runtime
        .with_storage_reader(|db| {
            let Some(stored) = db.host_input_draft(query.submission_id.as_str())? else {
                return Ok(None);
            };
            // Thread ownership and a deleted thread are decided by the run's thread, in
            // statements that stand alone (no transaction around the read).
            let owner = db.resolve_thread_of_run(&stored.run_id)?;
            Ok(owner
                .filter(|thread| thread == query.thread.as_str())
                .map(|_| stored))
        })
        .map_err(storage_error)?
        .flatten()
        .ok_or_else(not_found)?;
    if !is_queued(read.status) {
        return Err(error(
            ErrorCode::Conflict,
            "the input is no longer waiting to be delivered",
        ));
    }
    let Some(words) = read.text else {
        return Err(error(
            ErrorCode::LimitExceeded,
            "the input is too large to edit",
        ));
    };
    let words = masked(&words);
    if words.len() > MAX_INPUT_DRAFT_BYTES {
        return Err(error(
            ErrorCode::LimitExceeded,
            "the input is too large to edit",
        ));
    }
    let draft = InputDraft {
        thread: query.thread.clone(),
        submission_id: query.submission_id.clone(),
        run: RunId::new(&read.run_id).map_err(|_| not_found())?,
        state: state_of(read.status),
        text: words,
        image_count: read.image_count,
        attachment_count: read.attachment_count,
    };
    draft.validate().map_err(|_| not_found())?;
    Ok(draft)
}
