//! Side questions (`/btw`, ADR 0045 E10): one question put to the context a
//! thread's root run last sent to its model, answered without touching the
//! conversation.
//!
//! The question and its answer are never part of the conversation: nothing
//! reaches the thread's history, the input receipts or the durable events. The
//! operation lives in the core's memory only. Accepting it answers with an
//! [`OperationView`]; the answer text is read with [`GetBtw`], and
//! [`BtwChanged`](super::ExecutionEvent::BtwChanged) only says that the
//! operation changed (it never carries the text).
use crate::{
    ids::{OperationId, RunId, ThreadId},
    limits::{MAX_TEXT_BYTES, MAX_TITLE_BYTES},
    operation::{OperationState, OperationView},
    validation::{ValidationError, check_bytes, check_text},
};
use serde::{Deserialize, Serialize};

/// Most bytes of an answer a [`BtwView`] carries; a longer one is cut and
/// marked [`BtwView::truncated`].
pub const MAX_BTW_ANSWER_BYTES: usize = MAX_TEXT_BYTES;
/// Most bytes of the text that explains a failure.
pub const MAX_BTW_FAILURE_BYTES: usize = MAX_TITLE_BYTES;

/// Ask `question` about `thread`'s conversation. The same id with the same
/// request joins the operation that exists; another request under the id is a
/// `Conflict`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AskBtw {
    pub operation_id: OperationId,
    pub thread: ThreadId,
    pub question: String,
}

impl AskBtw {
    /// # Errors
    /// An empty or blank question, one that is too long or has a control character.
    pub fn validate(&self) -> Result<(), ValidationError> {
        check_text("question", &self.question, MAX_TEXT_BYTES, false)?;
        if self.question.trim().is_empty() {
            return Err(ValidationError::Empty { field: "question" });
        }
        Ok(())
    }
}

/// Stop waiting for the answer. The operation settles as cancelled.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CancelBtw {
    pub operation_id: OperationId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GetBtw {
    pub operation_id: OperationId,
}

/// The context an answer was drawn from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BtwSource {
    pub run: RunId,
    /// Whether the context already held the run's last final answer. When it
    /// did not, the newest tool round or reply of a running turn is missing.
    pub settled: bool,
}

/// What a client reads about a side question.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BtwView {
    pub operation: OperationView,
    pub thread: ThreadId,
    /// Known once the operation took its context.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<BtwSource>,
    /// The answer, Markdown, once the operation is ready.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer: Option<String>,
    /// The answer was cut at [`MAX_BTW_ANSWER_BYTES`].
    #[serde(default)]
    pub truncated: bool,
    /// Why the operation failed, for a person to read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<String>,
}

impl BtwView {
    pub fn validate(&self) -> Result<(), ValidationError> {
        self.operation.validate()?;
        if let Some(answer) = &self.answer {
            check_bytes("answer", answer, MAX_BTW_ANSWER_BYTES, true)?;
            if self.operation.state != OperationState::Ready {
                return Err(ValidationError::Inconsistent {
                    field: "answer",
                    reason: "only a ready side question has an answer",
                });
            }
        }
        if let Some(failure) = &self.failure {
            check_bytes("failure", failure, MAX_BTW_FAILURE_BYTES, true)?;
            if !matches!(self.operation.state, OperationState::Failed { .. }) {
                return Err(ValidationError::Inconsistent {
                    field: "failure",
                    reason: "only a failed side question has a failure",
                });
            }
        }
        Ok(())
    }
}
