//! Memory namespace, read side (ADR 0045, PR 13): the lessons of a project, as
//! the shared [`crate::management`] vocabulary names them. PR 14 adds the
//! commands that change a lesson, with the same ids, revisions and actions.
//!
//! Reads are operations (see [`super::diagnostics`]): the query names an
//! operation id, and the data comes with the `Ready` outcome. Nothing here
//! writes, validates, promotes or rejects a lesson.
use super::diagnostics::{BoundedText, OperationTarget, ReadOutcome};
use crate::{
    handshake::{Capability, WireLimits},
    ids::{MemoryId, OperationId, ProjectId},
    limits::{MAX_NAME_BYTES, MAX_REFERENCE_BYTES},
    management::{MemoryState, MemoryStatus},
    operation::OperationView,
    queries::PageRequest,
    validation::{ValidationError, check_bytes, check_count, check_line},
    views::Page,
};
use serde::{Deserialize, Serialize};

pub const COMMAND_OPERATIONS: &[&str] = &[];
pub const QUERY_OPERATIONS: &[&str] = &["list_memory", "get_memory", "get_memory_operation"];
pub const EVENT_TYPES: &[&str] = &["memory_operation_changed"];
/// Tagged choices inside a command's payload that classification names (none).
pub const NESTED_COMMANDS: &[super::NestedOperation] = &[];

/// Longest search text, in bytes.
pub const MAX_SEARCH_BYTES: usize = 256;
/// Longest [`MemorySummary::preview`], in bytes.
pub const MAX_PREVIEW_BYTES: usize = 240;

no_commands!(MemoryCommand);
no_results!(MemoryResult);

/// Who a lesson is about, which decides where a promoted lesson is used.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryScope {
    Project,
    Harness,
    User,
}

/// A lesson in a list: its state, and the start of its text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemorySummary {
    pub state: MemoryState,
    pub scope: MemoryScope,
    /// The first line of the lesson, cut to [`MAX_PREVIEW_BYTES`].
    pub preview: String,
}

/// One lesson with its bounded text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryDetail {
    pub state: MemoryState,
    pub scope: MemoryScope,
    /// The task the lesson came from.
    pub task_id: String,
    pub content: BoundedText,
    pub evidence: BoundedText,
}

impl MemoryDetail {
    pub fn validate(&self) -> Result<(), ValidationError> {
        check_line("task_id", &self.task_id, MAX_REFERENCE_BYTES, true)?;
        check_count("actions", self.state.actions.len(), 16)?;
        self.content.validate()?;
        self.evidence.validate()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListMemory {
    pub operation_id: OperationId,
    pub project: ProjectId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<MemoryStatus>,
    /// `Some(true)` lists favorites only, `Some(false)` the others.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub favorite: Option<bool>,
    /// Words the lesson text must contain. Every word is a literal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Not part of the request's identity (see [`super::diagnostics::GetRunContext`]).
    #[serde(default)]
    pub page: PageRequest,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GetMemory {
    pub operation_id: OperationId,
    pub project: ProjectId,
    pub id: MemoryId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum MemoryQuery {
    ListMemory(ListMemory),
    GetMemory(GetMemory),
    GetMemoryOperation(OperationTarget),
}

impl MemoryQuery {
    pub fn required_capability(&self) -> Capability {
        Capability::Queries
    }

    pub fn validate(&self, _limits: &WireLimits) -> Result<(), ValidationError> {
        match self {
            Self::ListMemory(list) => {
                if let Some(text) = &list.text {
                    check_bytes("text", text, MAX_SEARCH_BYTES, false)?;
                }
                list.page.validate()
            }
            Self::GetMemory(_) | Self::GetMemoryOperation(_) => Ok(()),
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
pub enum MemoryReply {
    List(Box<ReadOutcome<Page<MemorySummary>>>),
    Detail(Box<ReadOutcome<MemoryDetail>>),
    Operation(OperationView),
}

impl MemoryReply {
    pub fn validate(&self, _limits: &WireLimits) -> Result<(), ValidationError> {
        match self {
            Self::List(outcome) => outcome.validate_with(|page| {
                page.validate_with(|item| {
                    check_bytes("preview", &item.preview, MAX_PREVIEW_BYTES, true)?;
                    check_bytes("id", item.state.id.as_str(), MAX_NAME_BYTES, false)
                })
            }),
            Self::Detail(outcome) => outcome.validate_with(MemoryDetail::validate),
            Self::Operation(view) => view.validate(),
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
pub enum MemoryEvent {
    /// A read operation reached a final state; ask again for its data.
    MemoryOperationChanged { view: OperationView },
}

impl MemoryEvent {
    pub fn validate(&self) -> Result<(), ValidationError> {
        match self {
            Self::MemoryOperationChanged { view } => view.validate(),
        }
    }
}
