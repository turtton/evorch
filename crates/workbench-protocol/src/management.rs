//! Data the management areas share (ADR 0045): `memory`, `improvement` and
//! `evaluation`.
//!
//! The read side (PR 13) lists and shows these entities, the update side (PR 14
//! for `memory` and `improvement`, PR 17c for `evaluation`) changes them. Both
//! must speak about an entity in the same terms, so identity, state, revision
//! and what may be done to it are fixed here, in one place, before either side
//! is written. A later PR adds fields to the views of its own namespace; it does
//! not redefine these.
//!
//! Nothing here is an operation: no namespace lists a command, query or event
//! of these types yet, so the namespaces stay reserved. Lists are paged with
//! [`crate::views::Page`] and [`crate::queries::PageRequest`]; asynchronous
//! work uses [`crate::operation`]. Nothing here carries a body, a report or a
//! file path: text is fetched in bounded pieces by the namespace that owns it.
use crate::ids::{
    EvaluationGroupId, EvaluationJobId, EvaluationMemberId, ImprovementCandidateId, MemoryId,
};
use crate::operation::CancelReason;
use crate::wire::Revision;
use serde::{Deserialize, Serialize};

/// Why an action is not offered. Machine readable; the client words it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DenialReason {
    /// The entity's state does not allow the action.
    NotApplicableInState,
    /// A favorite memory must stop being a favorite before it is rejected.
    FavoriteMustBeCleared,
    /// The entity is already settled by an earlier decision.
    AlreadyResolved,
    /// Another operation on the entity is still running.
    OperationInProgress,
    /// The data is shown for reading only (a replay, an archived project).
    ReadOnly,
}

/// Whether one action may be done now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ActionState {
    Allowed,
    Denied { reason: DenialReason },
}

/// One action of an entity and whether it is offered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionAvailability<A> {
    pub action: A,
    pub state: ActionState,
}

// ---- memory ----------------------------------------------------------------

/// Where a lesson stands. Favorite is not a status: it is a separate flag and
/// does not change the status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryStatus {
    Candidate,
    Validated,
    Promoted,
    Rejected,
}

/// What a person can do to a lesson.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryAction {
    SetFavorite,
    ClearFavorite,
    Validate,
    Promote,
    Reject,
}

/// The identity and state of one lesson, without its text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryState {
    pub id: MemoryId,
    pub status: MemoryStatus,
    pub favorite: bool,
    /// Compared by an update (`expected_revision`); equality only.
    pub revision: Revision,
    pub actions: Vec<ActionAvailability<MemoryAction>>,
}

/// A lesson as an update names it: the id and the revision the client saw.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryRef {
    pub id: MemoryId,
    pub expected_revision: Revision,
}

// ---- improvement ------------------------------------------------------------

/// Where a candidate stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImprovementStatus {
    New,
    Reviewed,
    Dismissed,
}

/// What a person can do to a candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImprovementAction {
    MarkReviewed,
    Dismiss,
    /// Make a draft message from the candidate. The draft is never sent by the
    /// core on its own.
    Triage,
}

/// The identity and state of one candidate, without its evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImprovementState {
    pub id: ImprovementCandidateId,
    pub status: ImprovementStatus,
    pub revision: Revision,
    pub actions: Vec<ActionAvailability<ImprovementAction>>,
}

/// What a triage is made from: a candidate and the revision the client saw. It
/// names no path and no text; the core reads what it needs itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TriageSubject {
    pub candidate: ImprovementCandidateId,
    pub expected_revision: Revision,
}

// ---- evaluation --------------------------------------------------------------

/// Whether a stored artifact can be read. `Missing` is a normal state (never
/// produced, or removed); `Problem` is a failure to read what should be there.
/// A reader must not report one as the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactPresence {
    Present,
    Missing,
    Problem,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvaluationArtifactKind {
    Report,
    Replay,
}

/// A report or a replay of one member, and whether it can be read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluationArtifactRef {
    pub group: EvaluationGroupId,
    pub member: EvaluationMemberId,
    pub kind: EvaluationArtifactKind,
    pub presence: ArtifactPresence,
}

/// What an evaluation job does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvaluationJobKind {
    Run,
    GenerateReport,
    Rejudge,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EvaluationJobStatus {
    Queued,
    Running,
    Succeeded,
    /// Ended without producing what was asked for. A failed preflight stays
    /// failed even if a report was written afterwards.
    Failed,
    Cancelled {
        reason: CancelReason,
    },
}

impl EvaluationJobStatus {
    pub const fn is_final(&self) -> bool {
        !matches!(self, Self::Queued | Self::Running)
    }
}

/// The identity and state of one job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluationJobState {
    pub job: EvaluationJobId,
    pub group: EvaluationGroupId,
    /// Absent for a job of the whole group.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub member: Option<EvaluationMemberId>,
    pub kind: EvaluationJobKind,
    pub status: EvaluationJobStatus,
    pub revision: Revision,
}
