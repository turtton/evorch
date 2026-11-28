//! The words of one queued input, read on demand so that edit and restore work after a
//! client restarts (ADR 0045 E7).
//!
//! The reply is one bounded body: no paging, no body reference. Attachments are never
//! returned; only how many there were, so the client can ask the person to add them again.
use crate::{
    ids::{RunId, SubmissionId, ThreadId},
    limits::MAX_TEXT_BYTES,
    validation::{ValidationError, check_text},
    views::{InputReceiptState, validate_attachment_counts},
};
use serde::{Deserialize, Serialize};

/// Most bytes of words one reply carries. Words above it are refused, never cut.
pub const MAX_INPUT_DRAFT_BYTES: usize = MAX_TEXT_BYTES;

/// Read the words of `submission_id`, which must belong to `thread`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GetInputDraft {
    pub thread: ThreadId,
    pub submission_id: SubmissionId,
}

/// The words of an input as typed, with the state they were read in. Only an input in
/// `Accepted`, `Dropped` or `Unknown` is answered; any other state is a `Conflict`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InputDraft {
    pub thread: ThreadId,
    pub submission_id: SubmissionId,
    pub run: RunId,
    pub state: InputReceiptState,
    pub text: String,
    pub image_count: u32,
    pub attachment_count: u32,
}

impl InputDraft {
    pub fn validate(&self) -> Result<(), ValidationError> {
        check_text("text", &self.text, MAX_INPUT_DRAFT_BYTES, true)?;
        if !matches!(
            self.state,
            InputReceiptState::Accepted | InputReceiptState::Dropped | InputReceiptState::Unknown
        ) {
            return Err(ValidationError::OutOfRange {
                field: "state",
                reason: "only an input that can still be edited or restored has words to read",
            });
        }
        validate_attachment_counts(self.image_count, self.attachment_count)
    }
}
