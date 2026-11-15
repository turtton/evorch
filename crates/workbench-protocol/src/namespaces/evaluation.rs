//! Evaluation namespace, read side (ADR 0045, PR 13): which evaluation groups
//! exist, how their members stand, and the report a member left. PR 17c adds the
//! commands that run, report and rejudge, with the same ids and job vocabulary
//! of [`crate::management`]. Reads are operations (see [`super::diagnostics`]).
//!
//! The core reads the evaluation files; a view carries no path and no raw file
//! error. A file that is simply not there is [`ArtifactPresence::Missing`], one
//! that is there and cannot be used is [`ArtifactPresence::Problem`] with its
//! [`EvaluationProblemKind`]; the two are never reported as one another.
use super::diagnostics::{BoundedText, OperationTarget, ReadOutcome};
use crate::{
    handshake::{Capability, WireLimits},
    ids::{EvaluationGroupId, EvaluationMemberId, OperationId},
    limits::{MAX_NAME_BYTES, MAX_TITLE_BYTES},
    management::{ArtifactPresence, EvaluationArtifactRef},
    operation::OperationView,
    queries::PageRequest,
    validation::{ValidationError, check_bytes, check_count, check_line},
    views::Page,
    wire::Revision,
};
use serde::{Deserialize, Serialize};

pub const COMMAND_OPERATIONS: &[&str] = &[];
pub const QUERY_OPERATIONS: &[&str] = &[
    "list_evaluations",
    "get_evaluation",
    "get_evaluation_report",
    "get_evaluation_operation",
];
pub const EVENT_TYPES: &[&str] = &["evaluation_operation_changed"];
/// Tagged choices inside a command's payload that classification names (none).
pub const NESTED_COMMANDS: &[super::NestedOperation] = &[];

/// Most members one [`EvaluationGroupDetail`] lists.
pub const MAX_EVALUATION_MEMBERS: usize = 64;

no_commands!(EvaluationCommand);
no_results!(EvaluationResult);

/// Why an existing evaluation file could not be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvaluationProblemKind {
    Malformed,
    UnsupportedSchema,
    TooLarge,
    PermissionDenied,
    /// A symbolic link, a path outside the evaluation root or a file of the
    /// wrong type.
    UnsafePath,
    Io,
}

/// Where a group as a whole stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvaluationGroupPhase {
    Running,
    Finished,
    /// Every member ended, and some failed or has no report.
    Incomplete,
    /// No member is known to run, and the end of some was never seen.
    Unverified,
    /// Some members were not read, so whether the group is done cannot be said.
    Partial,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvaluationMemberPhase {
    Starting,
    RunningTrials,
    Reporting,
    Finished,
    FinishedWithoutReport,
    Failed,
    Unverified,
}

/// A group in a list. `presence` says whether its record could be read; the
/// other fields are there only for a `Present` group.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluationGroupSummary {
    pub group: EvaluationGroupId,
    pub presence: ArtifactPresence,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub problem: Option<EvaluationProblemKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<EvaluationGroupPhase>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub members_total: Option<u32>,
    /// Changes whenever the summary does; compare for equality only.
    pub revision: Revision,
}

impl EvaluationGroupSummary {
    pub fn validate(&self) -> Result<(), ValidationError> {
        for value in [&self.started_at, &self.finished_at].into_iter().flatten() {
            check_line("timestamp", value, MAX_NAME_BYTES, true)?;
        }
        Ok(())
    }
}

/// What a list answers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluationList {
    /// Newest first.
    pub groups: Page<EvaluationGroupSummary>,
    /// Groups the core did not list because of its own bound.
    pub omitted: u32,
    /// The directory scan hit its bound, so older groups may exist.
    pub scan_truncated: bool,
    /// The evaluation root itself could not be read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub problem: Option<EvaluationProblemKind>,
}

/// How many trials of a member are in each state.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluationTrialCounts {
    pub planned: u32,
    pub queued: u32,
    pub running: u32,
    pub finished: u32,
    pub failed: u32,
    pub interrupted: u32,
    pub unavailable: u32,
    pub unverified: u32,
}

/// One member: the evaluation of one thread.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluationMemberSummary {
    pub member: EvaluationMemberId,
    pub index: u32,
    pub thread_id: String,
    pub phase: EvaluationMemberPhase,
    pub trials: EvaluationTrialCounts,
    /// Whether `report.md` can be read ([`ArtifactPresence::Missing`] while it
    /// is being made or when nothing makes it).
    pub report: EvaluationArtifactRef,
    /// Whether at least one trial left the conversation a replay is read from.
    /// The replay itself is not part of this namespace yet.
    pub replay: EvaluationArtifactRef,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<String>,
    /// Files of the member that were present and unusable.
    pub problems: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluationGroupDetail {
    pub group: EvaluationGroupId,
    pub phase: EvaluationGroupPhase,
    pub started_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notice: Option<String>,
    /// The candidate configuration, bounded; absent when it is missing or unreadable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate: Option<BoundedText>,
    pub members: Vec<EvaluationMemberSummary>,
    /// Members the core did not list because of its own bound.
    pub members_omitted: u32,
    pub members_total: u32,
    pub revision: Revision,
}

impl EvaluationGroupDetail {
    pub fn validate(&self) -> Result<(), ValidationError> {
        check_line("started_at", &self.started_at, MAX_NAME_BYTES, true)?;
        if let Some(notice) = &self.notice {
            check_bytes("notice", notice, MAX_TITLE_BYTES, true)?;
        }
        if let Some(candidate) = &self.candidate {
            candidate.validate()?;
        }
        check_count("members", self.members.len(), MAX_EVALUATION_MEMBERS)?;
        for member in &self.members {
            check_line("thread_id", &member.thread_id, MAX_NAME_BYTES, true)?;
            if let Some(failure) = &member.failure {
                check_bytes("failure", failure, MAX_TITLE_BYTES, true)?;
            }
        }
        Ok(())
    }
}

/// The start of a member's `report.md`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluationReportText {
    pub artifact: EvaluationArtifactRef,
    /// Present only for a `Present` report.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<BoundedText>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub problem: Option<EvaluationProblemKind>,
}

impl EvaluationReportText {
    pub fn validate(&self) -> Result<(), ValidationError> {
        match (&self.text, self.artifact.presence) {
            (Some(text), ArtifactPresence::Present) => text.validate(),
            (None, ArtifactPresence::Present) => Err(ValidationError::Inconsistent {
                field: "text",
                reason: "a present report carries its text",
            }),
            (Some(_), _) => Err(ValidationError::Inconsistent {
                field: "text",
                reason: "only a present report carries text",
            }),
            (None, _) => Ok(()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListEvaluations {
    pub operation_id: OperationId,
    /// Not part of the request's identity (see [`super::diagnostics::GetRunContext`]).
    #[serde(default)]
    pub page: PageRequest,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GetEvaluation {
    pub operation_id: OperationId,
    pub group: EvaluationGroupId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GetEvaluationReport {
    pub operation_id: OperationId,
    pub group: EvaluationGroupId,
    pub member: EvaluationMemberId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum EvaluationQuery {
    ListEvaluations(ListEvaluations),
    GetEvaluation(GetEvaluation),
    GetEvaluationReport(GetEvaluationReport),
    GetEvaluationOperation(OperationTarget),
}

impl EvaluationQuery {
    pub fn required_capability(&self) -> Capability {
        Capability::Queries
    }

    pub fn validate(&self, _limits: &WireLimits) -> Result<(), ValidationError> {
        match self {
            Self::ListEvaluations(list) => list.page.validate(),
            Self::GetEvaluation(_)
            | Self::GetEvaluationReport(_)
            | Self::GetEvaluationOperation(_) => Ok(()),
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
pub enum EvaluationReply {
    List(Box<ReadOutcome<EvaluationList>>),
    Group(Box<ReadOutcome<EvaluationGroupDetail>>),
    Report(Box<ReadOutcome<EvaluationReportText>>),
    Operation(OperationView),
}

impl EvaluationReply {
    pub fn validate(&self, _limits: &WireLimits) -> Result<(), ValidationError> {
        match self {
            Self::List(outcome) => outcome
                .validate_with(|list| list.groups.validate_with(EvaluationGroupSummary::validate)),
            Self::Group(outcome) => outcome.validate_with(EvaluationGroupDetail::validate),
            Self::Report(outcome) => outcome.validate_with(EvaluationReportText::validate),
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
pub enum EvaluationEvent {
    /// A read operation reached a final state; ask again for its data.
    EvaluationOperationChanged { view: OperationView },
}

impl EvaluationEvent {
    pub fn validate(&self) -> Result<(), ValidationError> {
        match self {
            Self::EvaluationOperationChanged { view } => view.validate(),
        }
    }
}
