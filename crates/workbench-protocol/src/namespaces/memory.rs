//! Memory namespace (ADR 0045, PR 13 reads, PR 14 updates): the lessons of a
//! project, as the shared [`crate::management`] vocabulary names them.
//!
//! Reads are operations (see [`super::diagnostics`]): the query names an
//! operation id, and the data comes with the `Ready` outcome.
//!
//! # Updates
//!
//! A command names the project, the lesson and the revision the client read
//! ([`MemoryRef`]); the core applies it only if that revision is still current
//! and the lesson's state allows the action. The answer is an operation
//! ([`MemoryUpdateView`]) whose end is announced by
//! [`MemoryEvent::MemoryUpdateCompleted`] and readable with
//! `get_memory_update`:
//!
//! * `stale_revision`: the lesson changed since it was read. Read it again and
//!   decide again; the core never retries an old update by itself.
//! * `not_found`: no such lesson in the project (a lesson of another project is
//!   not told apart from one that does not exist).
//! * a refused action carries its [`DenialReason`] (a favorite lesson cannot be
//!   rejected until it stops being a favorite).
//! * An update that already is what it asks for ends `Ready` (nothing written).
//! * The same operation id with the same request joins the first one. An update
//!   whose outcome became unknown (the core restarted: `get_memory_update`
//!   answers `not_found`, or the instance epoch changed) is never counted as
//!   done and never resent as it is: ask for the lesson's state instead.
use super::diagnostics::{BoundedText, OperationTarget, ReadOutcome};
use crate::{
    errors::ErrorCode,
    handshake::{Capability, WireLimits},
    ids::{MemoryId, OperationId, ProjectId},
    limits::{MAX_NAME_BYTES, MAX_REFERENCE_BYTES},
    management::{DenialReason, MemoryAction, MemoryRef, MemoryState, MemoryStatus},
    operation::OperationView,
    queries::PageRequest,
    validation::{ValidationError, check_bytes, check_count, check_line},
    views::Page,
    wire::Revision,
};
use serde::{Deserialize, Serialize};

pub const COMMAND_OPERATIONS: &[&str] = &[
    "set_memory_favorite",
    "clear_memory_favorite",
    "validate_memory",
    "promote_memory",
    "reject_memory",
];
pub const QUERY_OPERATIONS: &[&str] = &[
    "list_memory",
    "get_memory",
    "get_memory_operation",
    "get_memory_update",
];
pub const EVENT_TYPES: &[&str] = &["memory_operation_changed", "memory_update_completed"];
/// Tagged choices inside a command's payload that classification names (none).
pub const NESTED_COMMANDS: &[super::NestedOperation] = &[];

/// Longest search text, in bytes.
pub const MAX_SEARCH_BYTES: usize = 256;
/// Longest [`MemorySummary::preview`], in bytes.
pub const MAX_PREVIEW_BYTES: usize = 240;

/// Longest sentence of [`MemoryUpdateView::message`], in bytes.
pub const MAX_UPDATE_MESSAGE_BYTES: usize = 512;

/// One update of a lesson: which project it is in, which lesson, and the
/// revision the client read. No other field: what the action does is its name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateMemory {
    pub operation_id: OperationId,
    pub project: ProjectId,
    pub target: MemoryRef,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum MemoryCommand {
    /// Mark the lesson as a favorite. The status does not change.
    SetMemoryFavorite(UpdateMemory),
    ClearMemoryFavorite(UpdateMemory),
    /// `candidate` to `validated`.
    ValidateMemory(UpdateMemory),
    /// `validated` to `promoted`.
    PromoteMemory(UpdateMemory),
    /// Refused while the lesson is a favorite.
    RejectMemory(UpdateMemory),
}

impl MemoryCommand {
    pub fn required_capability(&self) -> Capability {
        Capability::Commands
    }

    pub fn validate(&self) -> Result<(), ValidationError> {
        // Every field is a validated id or a number.
        Ok(())
    }

    pub fn update(&self) -> &UpdateMemory {
        match self {
            Self::SetMemoryFavorite(update)
            | Self::ClearMemoryFavorite(update)
            | Self::ValidateMemory(update)
            | Self::PromoteMemory(update)
            | Self::RejectMemory(update) => update,
        }
    }

    pub fn operation_id(&self) -> &OperationId {
        &self.update().operation_id
    }

    /// The action of the shared vocabulary the command asks for.
    pub fn action(&self) -> MemoryAction {
        match self {
            Self::SetMemoryFavorite(_) => MemoryAction::SetFavorite,
            Self::ClearMemoryFavorite(_) => MemoryAction::ClearFavorite,
            Self::ValidateMemory(_) => MemoryAction::Validate,
            Self::PromoteMemory(_) => MemoryAction::Promote,
            Self::RejectMemory(_) => MemoryAction::Reject,
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
pub enum MemoryResult {
    /// The update's operation, pending or settled.
    Operation(MemoryUpdateView),
}

/// An update and how it stands. The state of `operation` carries the failure
/// class; `denial` says why an action was refused, `message` is a bounded,
/// secret-free sentence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryUpdateView {
    pub operation: OperationView,
    pub project: ProjectId,
    pub id: MemoryId,
    pub action: MemoryAction,
    /// The lesson's revision after a `Ready` update (the unchanged one when
    /// nothing had to be written).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<Revision>,
    /// Whether the update wrote anything; absent until it is `Ready`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changed: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub denial: Option<DenialReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl MemoryUpdateView {
    pub fn validate(&self) -> Result<(), ValidationError> {
        self.operation.validate()?;
        if let Some(message) = &self.message {
            check_bytes("message", message, MAX_UPDATE_MESSAGE_BYTES, true)?;
        }
        let ready = self.operation.state == crate::operation::OperationState::Ready;
        if !ready && (self.revision.is_some() || self.changed.is_some()) {
            return Err(ValidationError::Inconsistent {
                field: "revision",
                reason: "only a ready update names a revision",
            });
        }
        if self.denial.is_some() && self.failure_code().is_none() {
            return Err(ValidationError::Inconsistent {
                field: "denial",
                reason: "only a failed update names a denial",
            });
        }
        Ok(())
    }

    /// The failure class when the update failed.
    pub fn failure_code(&self) -> Option<ErrorCode> {
        match self.operation.state {
            crate::operation::OperationState::Failed { code } => Some(code),
            _ => None,
        }
    }
}

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
    /// The update an earlier command started, whatever its state.
    GetMemoryUpdate(OperationTarget),
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
            Self::GetMemory(_) | Self::GetMemoryOperation(_) | Self::GetMemoryUpdate(_) => Ok(()),
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
    Update(Box<MemoryUpdateView>),
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
            Self::Update(view) => view.validate(),
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
    /// An update ended; also readable with `get_memory_update`. Read the lesson
    /// again for its new state.
    MemoryUpdateCompleted { update: MemoryUpdateView },
}

impl MemoryEvent {
    pub fn validate(&self) -> Result<(), ValidationError> {
        match self {
            Self::MemoryOperationChanged { view } => view.validate(),
            Self::MemoryUpdateCompleted { update } => update.validate(),
        }
    }
}
