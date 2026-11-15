//! The contract every asynchronous operation of the areas shares (ADR 0045).
//!
//! Handlers never wait (`workbench-host/src/areas/mod.rs`). Work that has to
//! wait, or that blocks on I/O, becomes an *operation*: the client names it
//! with an [`OperationId`], the handler answers at once with the operation's
//! [`OperationView`], and the outcome is read later with a query or an event of
//! the area. This module only defines the vocabulary. No namespace names an
//! operation of this kind yet (every area that needs one does so in its own
//! module, as `mcp` does with its older, area-specific form).
//!
//! # Rules a core follows
//!
//! * The same id with the same request joins the existing operation; the same
//!   id with another request is a `Conflict`. A client that did not see an
//!   answer asks for the operation again instead of resending its request, and
//!   never resends an update whose outcome became unknown across a restart (the
//!   instance epoch changed).
//! * The host refuses a request it cannot hold (queue full, draining) *before*
//!   accepting it. An accepted operation is never dropped silently: it ends as
//!   [`OperationState::Ready`], [`OperationState::Failed`] or
//!   [`OperationState::Cancelled`].
//! * **Restart permit.** The host's restart gate counts a permit per accepted
//!   command, and a planned restart cannot seal while one is outstanding. An
//!   operation keeps the permit it was accepted with while it works, up to the
//!   moment its outcome is settled.
//! * **Waiting for a person holds no permit.** A section that waits for the
//!   user (an authentication callback in a browser, a confirmation) must not
//!   hold a permit: the wait has no bound, and one permit would keep the
//!   restart from ever sealing and, because a preparing host refuses new work,
//!   also keep the user's own cancellation out. Such a section runs without a
//!   permit. A planned restart or a shutdown ends it, and the operation settles
//!   as [`OperationState::Cancelled`] with [`CancelReason::Restart`] or
//!   [`CancelReason::Shutdown`]. When the section ends by itself, the operation
//!   enters the gate again at the point where it writes its result; if the gate
//!   refuses, it settles as `Cancelled` and writes nothing.
//! * A page of operations or results is bounded: [`crate::views::Page`] and
//!   [`crate::queries::PageRequest`] are the only list shape.
use crate::errors::ErrorCode;
use crate::ids::OperationId;
use crate::validation::{ValidationError, check_line};
use crate::wire::Revision;
use serde::{Deserialize, Serialize};

/// Longest [`ResultRef`].
pub const MAX_RESULT_REF_BYTES: usize = 256;

/// Why an operation was cancelled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CancelReason {
    /// A client asked for it.
    Requested,
    /// A planned restart ended the wait for a person (see the module docs).
    Restart,
    /// The core is shutting down.
    Shutdown,
}

/// Where an operation stands. `Pending` is the only state that is not final.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OperationState {
    /// Accepted; the outcome is not known yet.
    Pending,
    /// Finished; the result, if there is one, is named by [`OperationView::result`].
    Ready,
    /// Finished without effect, or with an effect the failure class names. The
    /// text of the failure is never part of the state.
    Failed {
        code: ErrorCode,
    },
    Cancelled {
        reason: CancelReason,
    },
}

impl OperationState {
    /// Whether the outcome is settled.
    pub const fn is_final(&self) -> bool {
        !matches!(self, Self::Pending)
    }
}

/// An opaque name of what a finished operation produced (a report, a page of a
/// search, a saved revision). Only the area that issued it can resolve it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ResultRef(String);

impl ResultRef {
    /// # Errors
    /// The value is empty, too long or has a control character.
    pub fn new(value: impl Into<String>) -> Result<Self, ValidationError> {
        let value = value.into();
        check_line("ResultRef", &value, MAX_RESULT_REF_BYTES, false)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for ResultRef {
    type Error = ValidationError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<ResultRef> for String {
    fn from(value: ResultRef) -> Self {
        value.0
    }
}

/// What a client reads about an operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperationView {
    pub operation_id: OperationId,
    pub state: OperationState,
    /// Changes whenever the view does; compare for equality only.
    pub revision: Revision,
    /// Only a `Ready` operation names a result, and not every one has it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<ResultRef>,
}

impl OperationView {
    /// # Errors
    /// A result on an operation that is not `Ready`.
    pub fn validate(&self) -> Result<(), ValidationError> {
        if self.result.is_some() && self.state != OperationState::Ready {
            return Err(ValidationError::Inconsistent {
                field: "result",
                reason: "only a ready operation names a result",
            });
        }
        Ok(())
    }
}
