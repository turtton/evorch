//! Improvement namespace (ADR 0045, PR 13 reads, PR 14 updates): the
//! self-improvement candidates of a project, in the shared [`crate::management`]
//! vocabulary. Reads are operations (see [`super::diagnostics`]).
//!
//! # Updates
//!
//! A command names the project, the candidate and the revision the client read.
//! The answer is an operation ([`ImprovementUpdateView`]) whose end is announced
//! by [`ImprovementEvent::ImprovementUpdateCompleted`] and readable with
//! `get_improvement_update`. The failure classes are those of the memory
//! namespace (`stale_revision`: read again; `not_found`: also for a candidate
//! of another project; a refused action carries its [`DenialReason`]; an update
//! that already is what it asks for ends `Ready`), and an update whose outcome
//! became unknown is never counted as done: ask for the candidate's state.
//!
//! `triage_improvement` names no path and no text. The core renders a draft
//! of the candidate into its own draft directory under a file name it derives
//! from the candidate id, and attaches that file to the candidate. The draft
//! is never sent anywhere by the core; the view names it by file name only.
//!
//! A candidate's evidence is bounded text and its draft is named by file name
//! only; no path of the host is ever part of a view.
use super::diagnostics::{BoundedText, OperationTarget, ReadOutcome};
use crate::{
    handshake::{Capability, WireLimits},
    ids::{ImprovementCandidateId, OperationId, ProjectId, RunId},
    limits::{MAX_NAME_BYTES, MAX_TITLE_BYTES},
    management::{
        DenialReason, ImprovementAction, ImprovementState, ImprovementStatus, TriageSubject,
    },
    operation::{OperationState, OperationView},
    queries::PageRequest,
    validation::{ValidationError, check_bytes, check_count, check_line},
    views::Page,
    wire::Revision,
};
use serde::{Deserialize, Serialize};

pub const COMMAND_OPERATIONS: &[&str] = &[
    "mark_improvement_reviewed",
    "dismiss_improvement",
    "triage_improvement",
];
pub const QUERY_OPERATIONS: &[&str] = &[
    "list_improvement",
    "get_improvement",
    "get_improvement_operation",
    "get_improvement_update",
];
pub const EVENT_TYPES: &[&str] = &[
    "improvement_operation_changed",
    "improvement_update_completed",
];
/// Tagged choices inside a command's payload that classification names (none).
pub const NESTED_COMMANDS: &[super::NestedOperation] = &[];

/// Most recent runs one [`ImprovementDetail`] names.
pub const MAX_RECENT_RUNS: usize = 16;

/// Longest sentence of [`ImprovementUpdateView::message`], in bytes.
pub const MAX_UPDATE_MESSAGE_BYTES: usize = 512;

/// Review or dismiss one candidate, at the revision the client read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateImprovement {
    pub operation_id: OperationId,
    pub project: ProjectId,
    pub candidate: ImprovementCandidateId,
    pub expected_revision: Revision,
}

/// Make a draft from one candidate (see the module documentation).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TriageImprovement {
    pub operation_id: OperationId,
    pub project: ProjectId,
    pub subject: TriageSubject,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ImprovementCommand {
    MarkImprovementReviewed(UpdateImprovement),
    DismissImprovement(UpdateImprovement),
    TriageImprovement(TriageImprovement),
}

impl ImprovementCommand {
    pub fn required_capability(&self) -> Capability {
        Capability::Commands
    }

    pub fn validate(&self) -> Result<(), ValidationError> {
        // Every field is a validated id or a number.
        Ok(())
    }

    pub fn operation_id(&self) -> &OperationId {
        match self {
            Self::MarkImprovementReviewed(update) | Self::DismissImprovement(update) => {
                &update.operation_id
            }
            Self::TriageImprovement(triage) => &triage.operation_id,
        }
    }

    pub fn project(&self) -> &ProjectId {
        match self {
            Self::MarkImprovementReviewed(update) | Self::DismissImprovement(update) => {
                &update.project
            }
            Self::TriageImprovement(triage) => &triage.project,
        }
    }

    pub fn candidate(&self) -> &ImprovementCandidateId {
        match self {
            Self::MarkImprovementReviewed(update) | Self::DismissImprovement(update) => {
                &update.candidate
            }
            Self::TriageImprovement(triage) => &triage.subject.candidate,
        }
    }

    pub fn expected_revision(&self) -> Revision {
        match self {
            Self::MarkImprovementReviewed(update) | Self::DismissImprovement(update) => {
                update.expected_revision
            }
            Self::TriageImprovement(triage) => triage.subject.expected_revision,
        }
    }

    /// The action of the shared vocabulary the command asks for.
    pub fn action(&self) -> ImprovementAction {
        match self {
            Self::MarkImprovementReviewed(_) => ImprovementAction::MarkReviewed,
            Self::DismissImprovement(_) => ImprovementAction::Dismiss,
            Self::TriageImprovement(_) => ImprovementAction::Triage,
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
pub enum ImprovementResult {
    /// The update's operation, pending or settled.
    Operation(ImprovementUpdateView),
}

/// An update and how it stands (see [`super::memory::MemoryUpdateView`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImprovementUpdateView {
    pub operation: OperationView,
    pub project: ProjectId,
    pub candidate: ImprovementCandidateId,
    pub action: ImprovementAction,
    /// The candidate's revision after a `Ready` update.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<Revision>,
    /// Whether the update wrote anything; absent until it is `Ready`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changed: Option<bool>,
    /// The draft a `Ready` triage attached, by file name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub draft_file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub denial: Option<DenialReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl ImprovementUpdateView {
    pub fn validate(&self) -> Result<(), ValidationError> {
        self.operation.validate()?;
        if let Some(message) = &self.message {
            check_bytes("message", message, MAX_UPDATE_MESSAGE_BYTES, true)?;
        }
        if let Some(file) = &self.draft_file {
            check_line("draft_file", file, MAX_NAME_BYTES, false)?;
        }
        let ready = self.operation.state == OperationState::Ready;
        if !ready
            && (self.revision.is_some() || self.changed.is_some() || self.draft_file.is_some())
        {
            return Err(ValidationError::Inconsistent {
                field: "revision",
                reason: "only a ready update names a revision or a draft",
            });
        }
        if self.denial.is_some() && !matches!(self.operation.state, OperationState::Failed { .. }) {
            return Err(ValidationError::Inconsistent {
                field: "denial",
                reason: "only a failed update names a denial",
            });
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImprovementSource {
    Diagnostic,
    Lesson,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImprovementSeverity {
    Info,
    Warning,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImprovementSummary {
    pub state: ImprovementState,
    pub source: ImprovementSource,
    pub severity: ImprovementSeverity,
    pub code: String,
    pub title: String,
    pub occurrences: u32,
    pub last_seen_ms: u64,
}

impl ImprovementSummary {
    pub fn validate(&self) -> Result<(), ValidationError> {
        check_line("code", &self.code, MAX_NAME_BYTES, true)?;
        check_line("title", &self.title, MAX_TITLE_BYTES, true)?;
        check_count("actions", self.state.actions.len(), 16)
    }
}

/// What a list answers: the candidates, and whether the core collects any.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImprovementList {
    /// `[self_improvement] enabled` as the core started with it. While it is
    /// off, candidates are not collected (older ones may still be listed).
    pub enabled: bool,
    pub candidates: Page<ImprovementSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImprovementDetail {
    pub summary: ImprovementSummary,
    pub evidence: BoundedText,
    /// The draft's file name, when one was attached; never its directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub draft_file: Option<String>,
    /// The run of the first occurrence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<RunId>,
    /// Distinct runs of the most recent occurrences, oldest first.
    pub recent_runs: Vec<RunId>,
    pub created_ms: u64,
    pub reopened_count: u32,
}

impl ImprovementDetail {
    pub fn validate(&self) -> Result<(), ValidationError> {
        self.summary.validate()?;
        self.evidence.validate()?;
        if let Some(file) = &self.draft_file {
            check_line("draft_file", file, MAX_NAME_BYTES, false)?;
        }
        check_count("recent_runs", self.recent_runs.len(), MAX_RECENT_RUNS)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListImprovement {
    pub operation_id: OperationId,
    pub project: ProjectId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<ImprovementStatus>,
    /// Not part of the request's identity (see [`super::diagnostics::GetRunContext`]).
    #[serde(default)]
    pub page: PageRequest,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GetImprovement {
    pub operation_id: OperationId,
    pub project: ProjectId,
    pub candidate: ImprovementCandidateId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ImprovementQuery {
    ListImprovement(ListImprovement),
    GetImprovement(GetImprovement),
    GetImprovementOperation(OperationTarget),
    /// The update an earlier command started, whatever its state.
    GetImprovementUpdate(OperationTarget),
}

impl ImprovementQuery {
    pub fn required_capability(&self) -> Capability {
        Capability::Queries
    }

    pub fn validate(&self, _limits: &WireLimits) -> Result<(), ValidationError> {
        match self {
            Self::ListImprovement(list) => list.page.validate(),
            Self::GetImprovement(_)
            | Self::GetImprovementOperation(_)
            | Self::GetImprovementUpdate(_) => Ok(()),
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
pub enum ImprovementReply {
    List(Box<ReadOutcome<ImprovementList>>),
    Detail(Box<ReadOutcome<ImprovementDetail>>),
    Operation(OperationView),
    Update(Box<ImprovementUpdateView>),
}

impl ImprovementReply {
    pub fn validate(&self, _limits: &WireLimits) -> Result<(), ValidationError> {
        match self {
            Self::List(outcome) => outcome
                .validate_with(|list| list.candidates.validate_with(ImprovementSummary::validate)),
            Self::Detail(outcome) => outcome.validate_with(ImprovementDetail::validate),
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
pub enum ImprovementEvent {
    /// A read operation reached a final state; ask again for its data.
    ImprovementOperationChanged { view: OperationView },
    /// An update ended; also readable with `get_improvement_update`.
    ImprovementUpdateCompleted { update: ImprovementUpdateView },
}

impl ImprovementEvent {
    pub fn validate(&self) -> Result<(), ValidationError> {
        match self {
            Self::ImprovementOperationChanged { view } => view.validate(),
            Self::ImprovementUpdateCompleted { update } => update.validate(),
        }
    }
}
