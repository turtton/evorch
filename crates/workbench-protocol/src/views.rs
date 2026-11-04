//! Read models. Stable DTOs, not runtime or storage types.
//!
//! Text in views may be shown to remote clients, so the core must pass it
//! through secret masking before building a view; this crate only bounds it.

use crate::envelope::Provenance;
use crate::ids::{
    ApprovalId, BodyId, PageCursor, ProjectId, QuestionId, RunId, SubmissionId, ThreadId,
};
use crate::limits::{
    MAX_INLINE_MESSAGE_BYTES, MAX_NAME_BYTES, MAX_OPTION_BYTES, MAX_OPTIONS, MAX_PAGE_SIZE,
    MAX_TEXT_BYTES, MAX_TITLE_BYTES, MIN_BODY_CHUNK_BYTES,
};
use crate::validation::{ValidationError, check_bytes, check_count};
use crate::wire::{ByteOffset, InstanceEpoch, Revision, Sequence, UnixMillis};
use serde::{Deserialize, Serialize};

/// One page of a listing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Page<T> {
    pub items: Vec<T>,
    /// Present when more items follow. Opaque: valid only for the same query.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<PageCursor>,
    /// Revision of the listed collection when the page was cut, so a reader can
    /// tell a page from a later state.
    pub snapshot_revision: Revision,
}

impl<T> Page<T> {
    /// Reject a page larger than the protocol allows.
    pub fn validate(&self) -> Result<(), ValidationError> {
        check_count("items", self.items.len(), MAX_PAGE_SIZE as usize)
    }

    /// Reject an oversized page, then check every item with `check`.
    pub fn validate_with(
        &self,
        check: impl Fn(&T) -> Result<(), ValidationError>,
    ) -> Result<(), ValidationError> {
        self.validate()?;
        self.items.iter().try_for_each(check)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectSummary {
    pub id: ProjectId,
    pub display_name: String,
    pub thread_count: u32,
    pub revision: Revision,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThreadState {
    Idle,
    Running,
    WaitingForUser,
    WaitingForApproval,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThreadSummary {
    pub id: ThreadId,
    pub project: ProjectId,
    pub title: String,
    pub state: ThreadState,
    /// The lead thread when this is a lane (ADR 0044).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lead_thread: Option<ThreadId>,
    pub revision: Revision,
    pub updated_at: UnixMillis,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThreadView {
    pub summary: ThreadSummary,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_run: Option<RunId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_run: Option<RunId>,
    pub pending_questions: u32,
    pub pending_approvals: u32,
    pub queued_follow_ups: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunRole {
    Orchestrator,
    Worker,
    Planner,
    Reviewer,
    Advisor,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunPhase {
    Pending,
    Running,
    WaitingForUser,
    WaitingForApproval,
    Done,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunSummary {
    pub id: RunId,
    pub thread: ThreadId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_run: Option<RunId>,
    pub role: RunRole,
    pub phase: RunPhase,
    pub revision: Revision,
}

/// The run and thread that raised a question, and the conversation's root run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuestionRequester {
    pub run: RunId,
    pub thread: ThreadId,
    pub root_run: RunId,
}

/// Who must answer (ADR 0044: a lane's question may go to its lead).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum QuestionTarget {
    User,
    Lead {
        lead_thread: ThreadId,
        lead_run: RunId,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuestionAnswer {
    pub text: String,
    pub answered_by: Provenance,
}

impl QuestionAnswer {
    pub fn validate(&self) -> Result<(), ValidationError> {
        check_bytes("answer.text", &self.text, MAX_TEXT_BYTES, true)?;
        self.answered_by.validate()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuestionView {
    pub id: QuestionId,
    pub requester: QuestionRequester,
    pub target: QuestionTarget,
    /// Changes when the question is rerouted by a lead.
    pub management_revision: Revision,
    pub routing_revision: Revision,
    /// A blocking question holds its run until answered.
    pub blocking: bool,
    pub title: String,
    pub options: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer: Option<QuestionAnswer>,
    /// The revision `AnswerUserQuestion` must quote.
    pub revision: Revision,
    /// Who raised the question: an agent run.
    pub raised_by: Provenance,
}

impl QuestionView {
    pub fn validate(&self) -> Result<(), ValidationError> {
        // Agent-authored text: bounded, not restricted to a character set.
        check_bytes("title", &self.title, MAX_TITLE_BYTES, true)?;
        check_count("options", self.options.len(), MAX_OPTIONS)?;
        self.options
            .iter()
            .try_for_each(|o| check_bytes("options", o, MAX_OPTION_BYTES, true))?;
        self.raised_by.validate()?;
        if let Some(answer) = &self.answer {
            answer.validate()?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalState {
    Pending,
    Approved,
    Denied,
    /// The request is gone (run ended or core restarted); it can no longer be decided.
    Expired,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalView {
    pub id: ApprovalId,
    /// Core instance that issued the request; a decision must quote it.
    pub core_epoch: InstanceEpoch,
    pub revision: Revision,
    pub thread: ThreadId,
    pub run: RunId,
    pub tool_name: String,
    pub summary: String,
    pub state: ApprovalState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decided_by: Option<Provenance>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputReceiptState {
    /// The runtime took it; the run may still hold it in its inbox.
    Accepted,
    Withdrawn,
    /// A committed context contains it.
    Delivered,
    /// Its run ended before reading it.
    Dropped,
    /// The core cannot tell whether the agent received it.
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InputReceipt {
    pub submission_id: SubmissionId,
    pub thread: ThreadId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<RunId>,
    pub state: InputReceiptState,
    pub revision: Revision,
    pub sent_by: Provenance,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    User,
    Assistant,
    Tool,
}

/// A body stored apart from its message because it is large.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BodyRef {
    pub body_id: BodyId,
    pub total_bytes: ByteOffset,
}

/// Message text, inline when small and by reference otherwise.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum MessageContent {
    Inline { text: String },
    Body(BodyRef),
}

/// A message that is part of committed history. Streaming deltas are never exposed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommittedMessage {
    pub thread: ThreadId,
    pub run: RunId,
    /// Position within the run's committed history.
    pub index: Sequence,
    pub role: MessageRole,
    pub content: MessageContent,
}

impl CommittedMessage {
    pub fn validate(&self) -> Result<(), ValidationError> {
        match &self.content {
            MessageContent::Inline { text } => {
                check_bytes("content", text, MAX_INLINE_MESSAGE_BYTES, true)
            }
            MessageContent::Body(_) => Ok(()),
        }
    }
}

/// One slice of a body. Offsets are in bytes and always fall on character
/// boundaries, so `text` is valid UTF-8 by itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BodyChunk {
    pub body_id: BodyId,
    pub offset: ByteOffset,
    pub text: String,
    pub total_bytes: ByteOffset,
    /// Where the next chunk starts; absent at the end of the body.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_offset: Option<ByteOffset>,
}

impl BodyChunk {
    /// Cut the chunk starting at `offset`, at most `max_bytes` long, backing off
    /// to a character boundary. This is the one place that rule lives.
    pub fn slice(
        body_id: BodyId,
        body: &str,
        offset: ByteOffset,
        max_bytes: u32,
    ) -> Result<Self, ValidationError> {
        if max_bytes < MIN_BODY_CHUNK_BYTES {
            return Err(ValidationError::OutOfRange {
                field: "max_bytes",
                reason: "smaller than one UTF-8 scalar",
            });
        }
        let start = usize::try_from(offset.get())
            .ok()
            .filter(|start| *start <= body.len())
            .ok_or(ValidationError::OutOfRange {
                field: "offset",
                reason: "beyond the end of the body",
            })?;
        if !body.is_char_boundary(start) {
            return Err(ValidationError::OutOfRange {
                field: "offset",
                reason: "not on a character boundary",
            });
        }
        let mut end = body.len().min(start.saturating_add(max_bytes as usize));
        while !body.is_char_boundary(end) {
            end -= 1;
        }
        Ok(Self {
            body_id,
            offset,
            text: body[start..end].to_owned(),
            total_bytes: ByteOffset(body.len() as u64),
            next_offset: (end < body.len()).then_some(ByteOffset(end as u64)),
        })
    }

    pub fn validate(&self, max_chunk_bytes: u32) -> Result<(), ValidationError> {
        check_bytes("text", &self.text, max_chunk_bytes as usize, true)?;
        let end = self
            .offset
            .get()
            .checked_add(self.text.len() as u64)
            .ok_or(ValidationError::OutOfRange {
                field: "offset",
                reason: "offset plus chunk length overflows",
            })?;
        if end > self.total_bytes.get() {
            return Err(ValidationError::Inconsistent {
                field: "total_bytes",
                reason: "chunk extends past the end of the body",
            });
        }
        if self.text.is_empty() && self.next_offset.is_some() {
            // A reader following next_offset would loop without making progress.
            return Err(ValidationError::Inconsistent {
                field: "text",
                reason: "a chunk that is not the last must carry text",
            });
        }
        match self.next_offset {
            Some(next) if next.get() != end || end >= self.total_bytes.get() => {
                Err(ValidationError::Inconsistent {
                    field: "next_offset",
                    reason: "must equal the chunk end and lie inside the body",
                })
            }
            None if end != self.total_bytes.get() => Err(ValidationError::Inconsistent {
                field: "next_offset",
                reason: "missing before the end of the body",
            }),
            _ => Ok(()),
        }
    }
}

impl ProjectSummary {
    pub fn validate(&self) -> Result<(), ValidationError> {
        check_bytes("display_name", &self.display_name, MAX_TITLE_BYTES, true)
    }
}

impl ThreadSummary {
    pub fn validate(&self) -> Result<(), ValidationError> {
        check_bytes("title", &self.title, MAX_TITLE_BYTES, true)
    }
}

impl ThreadView {
    pub fn validate(&self) -> Result<(), ValidationError> {
        self.summary.validate()
    }
}

impl ApprovalView {
    pub fn validate(&self) -> Result<(), ValidationError> {
        check_bytes("tool_name", &self.tool_name, MAX_NAME_BYTES, true)?;
        check_bytes("summary", &self.summary, MAX_INLINE_MESSAGE_BYTES, true)?;
        self.decided_by
            .as_ref()
            .map_or(Ok(()), Provenance::validate)
    }
}

impl InputReceipt {
    pub fn validate(&self) -> Result<(), ValidationError> {
        self.sent_by.validate()
    }
}
