//! Lane attention read model (ADR 0044 Phase 2): what a lead has to act on across
//! the lanes it manages.
//!
//! The core derives the list from its lane ledger every time it is asked; the
//! list is not a notification log and nothing here records that it was read. A
//! client that lost events asks again and gets the same answer a client that
//! never lost any would.
//!
//! Naming: every type is prefixed `Lane` so it cannot be mistaken for the lane
//! fields other views already carry (`ThreadSummary::lead_thread`,
//! [`QuestionTarget`](crate::views::QuestionTarget), `WaitingLane`).
//!
//! These types only describe what may be said. Which operations a client
//! offers for an item is up to the client and to what the core implements;
//! [`LaneAttentionAction`] lists the operations an item may *allow*, not ones
//! any client must draw.

use crate::ids::{QuestionId, ThreadId, validate_id};
use crate::limits::{MAX_ID_BYTES, MAX_TITLE_BYTES};
use crate::validation::{ValidationError, check_bytes, check_count, check_line};
use crate::wire::{Revision, Sequence, UnixMillis};
use serde::{Deserialize, Serialize};

/// Most items on one page.
pub const MAX_LANE_ATTENTION_ITEMS: usize = 20;
/// Most evidence references on one item.
pub const MAX_LANE_ATTENTION_REFS: usize = 8;
/// Longest evidence or failure reference, in bytes.
pub const MAX_LANE_ATTENTION_REF_BYTES: usize = 512;
/// Most actions one item allows.
pub const MAX_LANE_ATTENTION_ACTIONS: usize = 8;
/// Longest page cursor, in bytes.
pub const MAX_LANE_ATTENTION_CURSOR_BYTES: usize = 512;

macro_rules! lane_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash,
                 serde::Serialize, serde::Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);

        impl $name {
            /// Build an id, rejecting malformed values.
            pub fn new(value: impl Into<String>) -> Result<Self, ValidationError> {
                let value = value.into();
                validate_id(stringify!($name), &value)?;
                Ok(Self(value))
            }

            /// The id as text.
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl TryFrom<String> for $name {
            type Error = ValidationError;
            fn try_from(value: String) -> Result<Self, Self::Error> {
                Self::new(value)
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> Self {
                value.0
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

lane_id!(
    /// A lane (ADR 0044): an independent Orchestrator thread a lead manages.
    LaneId
);
lane_id!(
    /// One attention item. Stable while the cause and the lane's management
    /// relation are unchanged, so a client can keep selection across queries.
    LaneAttentionItemId
);
lane_id!(
    /// One stall episode of a lane: a stretch without observable progress.
    LaneEpisodeId
);
lane_id!(
    /// The state of the lane ledger one page was cut from.
    LaneAttentionSnapshot
);

/// Opaque position in a lane attention listing. Valid only for the same filter
/// and snapshot; the core answers a stale one with `stale_cursor`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct LaneAttentionCursor(String);

impl LaneAttentionCursor {
    pub fn new(value: impl Into<String>) -> Result<Self, ValidationError> {
        let value = value.into();
        check_line(
            "LaneAttentionCursor",
            &value,
            MAX_LANE_ATTENTION_CURSOR_BYTES,
            false,
        )?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for LaneAttentionCursor {
    type Error = ValidationError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<LaneAttentionCursor> for String {
    fn from(value: LaneAttentionCursor) -> Self {
        value.0
    }
}

/// Who answers a lane's question.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum LaneQuestionResponder {
    /// The person at the client.
    User,
    /// The lead thread, through its lane tools. A client does not draw an
    /// answer control for this: the core refuses a person's answer to it.
    Lead { lead_thread: ThreadId },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaneQuestionCapacity {
    Requester,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaneReadyState {
    /// No fixed artifact is named and nothing was reviewed.
    NoEvidence,
    /// A fixed artifact is named; no independent review exists.
    Unreviewed,
    ReviewInProgress,
    /// The finished review rejected the Ready.
    ReviewRejected,
    /// The Reviewer ended without a complete submission; not a rejection.
    ReviewIncomplete,
    /// The review approved; the lead has not closed.
    AwaitingClose,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaneStallClassification {
    NoReport,
    WorkStalled,
    WaitingQuestion,
    SilentDone,
    RestoreUnavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaneNudgeState {
    NotRequested,
    Pending,
    Accepted,
    Suppressed,
    Obsolete,
    Failed,
}

/// Why a lane is listed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum LaneAttentionKind {
    QuestionPending {
        question: QuestionId,
        responder: LaneQuestionResponder,
        routing_revision: Revision,
    },
    Blocked {
        /// Position of the lane report that said so.
        report_seq: Sequence,
    },
    Failed {
        report_seq: Sequence,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        episode: Option<LaneEpisodeId>,
    },
    /// A held notice could not bring its destination back.
    RestoreFailed {
        destination_thread: ThreadId,
        failure_ref: String,
        attempts: u32,
        suppressed: bool,
    },
    /// The lane reported Ready and its lead has not closed it. Not a stall of the work,
    /// and no state here stops the lead from closing.
    ReadyUnclosed {
        ready_seq: Sequence,
        readiness: LaneReadyState,
    },
    QuestionCapacityBlocked {
        capacity: LaneQuestionCapacity,
        limit: u32,
    },
    /// Wakes of the lead are paused after wakes that brought no progress.
    WakeSuppressed { guard_revision: Revision },
    /// A Stop on the destination holds notices that are not lost.
    StopPending {
        destination_thread: ThreadId,
        #[serde(with = "crate::wire")]
        pending_count: u64,
    },
    Stall {
        episode: LaneEpisodeId,
        classification: LaneStallClassification,
        nudge: LaneNudgeState,
    },
}

/// An operation an item allows. A client draws only those it implements.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaneAttentionAction {
    /// The lead answers a question through its lane tools.
    Answer,
    /// The lead passes a question to the user.
    RouteToUser,
    /// Continue the lead's conversation.
    Continue,
    /// Look at the lane.
    Inspect,
    /// Start a verification of the lane's ready claim.
    Verify,
    /// Close the lane after a verification.
    Close,
    /// Deliver held notices again.
    Resume,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaneAttentionItem {
    pub id: LaneAttentionItemId,
    pub lane: LaneId,
    pub lead_thread: ThreadId,
    /// The lane's management relation this item belongs to; an older one is
    /// never current.
    pub management_revision: Revision,
    /// Changes when the cause changes, so a client can tell a changed item from
    /// the one it holds.
    pub revision: Revision,
    pub kind: LaneAttentionKind,
    /// When the cause began. Display only.
    pub since: UnixMillis,
    /// Bounded text written by an agent or by the core.
    pub summary: String,
    pub evidence_refs: Vec<String>,
    pub available_actions: Vec<LaneAttentionAction>,
}

impl LaneAttentionItem {
    pub fn validate(&self) -> Result<(), ValidationError> {
        check_bytes("summary", &self.summary, MAX_TITLE_BYTES, true)?;
        check_count(
            "evidence_refs",
            self.evidence_refs.len(),
            MAX_LANE_ATTENTION_REFS,
        )?;
        for reference in &self.evidence_refs {
            check_bytes(
                "evidence_refs",
                reference,
                MAX_LANE_ATTENTION_REF_BYTES,
                true,
            )?;
        }
        check_count(
            "available_actions",
            self.available_actions.len(),
            MAX_LANE_ATTENTION_ACTIONS,
        )?;
        if let LaneAttentionKind::RestoreFailed { failure_ref, .. } = &self.kind {
            check_bytes(
                "failure_ref",
                failure_ref,
                MAX_LANE_ATTENTION_REF_BYTES,
                true,
            )?;
        }
        Ok(())
    }
}

/// One page of attention items, cut from one snapshot of the ledger.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaneAttentionPage {
    pub items: Vec<LaneAttentionItem>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<LaneAttentionCursor>,
    pub snapshot: LaneAttentionSnapshot,
}

impl LaneAttentionPage {
    pub fn validate(&self) -> Result<(), ValidationError> {
        check_count("items", self.items.len(), MAX_LANE_ATTENTION_ITEMS)?;
        self.items.iter().try_for_each(LaneAttentionItem::validate)
    }
}

const _: () = assert!(MAX_ID_BYTES >= 64, "a snapshot digest must fit an id");
