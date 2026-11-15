//! Improvement namespace, read side (ADR 0045, PR 13): the self-improvement
//! candidates of a project, in the shared [`crate::management`] vocabulary. PR 14
//! adds the commands (review, dismiss, triage). Reads are operations (see
//! [`super::diagnostics`]).
//!
//! A candidate's evidence is bounded text and its draft is named by file name
//! only; no path of the host is ever part of a view.
use super::diagnostics::{BoundedText, OperationTarget, ReadOutcome};
use crate::{
    handshake::{Capability, WireLimits},
    ids::{ImprovementCandidateId, OperationId, ProjectId, RunId},
    limits::{MAX_NAME_BYTES, MAX_TITLE_BYTES},
    management::{ImprovementState, ImprovementStatus},
    operation::OperationView,
    queries::PageRequest,
    validation::{ValidationError, check_count, check_line},
    views::Page,
};
use serde::{Deserialize, Serialize};

pub const COMMAND_OPERATIONS: &[&str] = &[];
pub const QUERY_OPERATIONS: &[&str] = &[
    "list_improvement",
    "get_improvement",
    "get_improvement_operation",
];
pub const EVENT_TYPES: &[&str] = &["improvement_operation_changed"];
/// Tagged choices inside a command's payload that classification names (none).
pub const NESTED_COMMANDS: &[super::NestedOperation] = &[];

/// Most recent runs one [`ImprovementDetail`] names.
pub const MAX_RECENT_RUNS: usize = 16;

no_commands!(ImprovementCommand);
no_results!(ImprovementResult);

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
}

impl ImprovementQuery {
    pub fn required_capability(&self) -> Capability {
        Capability::Queries
    }

    pub fn validate(&self, _limits: &WireLimits) -> Result<(), ValidationError> {
        match self {
            Self::ListImprovement(list) => list.page.validate(),
            Self::GetImprovement(_) | Self::GetImprovementOperation(_) => Ok(()),
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
}

impl ImprovementReply {
    pub fn validate(&self, _limits: &WireLimits) -> Result<(), ValidationError> {
        match self {
            Self::List(outcome) => outcome
                .validate_with(|list| list.candidates.validate_with(ImprovementSummary::validate)),
            Self::Detail(outcome) => outcome.validate_with(ImprovementDetail::validate),
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
pub enum ImprovementEvent {
    /// A read operation reached a final state; ask again for its data.
    ImprovementOperationChanged { view: OperationView },
}

impl ImprovementEvent {
    pub fn validate(&self) -> Result<(), ValidationError> {
        match self {
            Self::ImprovementOperationChanged { view } => view.validate(),
        }
    }
}
