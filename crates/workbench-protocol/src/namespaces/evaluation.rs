//! Evaluation namespace (ADR 0045, PR 13 and PR 17c): which evaluation groups
//! exist, how their members stand, the report a member left, and the jobs that
//! run an evaluation, make its report or judge it again. Reads are operations
//! (see [`super::diagnostics`]); the jobs use the job vocabulary of
//! [`crate::management`].
//!
//! # Jobs
//!
//! `start_evaluation_run` and `start_evaluation_report` name an
//! [`OperationId`] and are answered at once with the job's
//! [`EvaluationJobView`]; the work runs in the core. A finished replay
//! publishes [`EvaluationEvent::EvaluationJobProgress`]; the end publishes
//! [`EvaluationEvent::EvaluationJobSettled`] and is readable with
//! `get_evaluation_job`.
//!
//! * **Idempotent.** The same id with the same request joins the first job
//!   (nothing runs twice); the same id with another request is a `conflict`.
//!   A job the core no longer holds (it restarted) answers `not_found`, and its
//!   outcome is unknown: look at the evaluation directory before running it
//!   again. A client never resends a command whose reply it lost.
//! * **Names, not paths.** A job names the directories it writes or reads by
//!   their name under the evaluation root ([`validate_target_name`]); a client
//!   never sends a path. The overlay of a run travels as its text.
//! * **Effect.** [`EvaluationJobView::effect`] says what the job left on disk:
//!   `none`, `partial` (a failed or cancelled job whose output directory was
//!   begun), `complete` (the job ended `ready`) or `unknown` (the job's task
//!   broke off in a way that does not say what was written). A failed
//!   preflight stays `failed` even when its evidence was reported.
//! * **Cancel.** `cancel_evaluation_job` stops the job where it is; the files
//!   it already wrote stay, and the workspaces it made are removed with
//!   `evorch eval clean`. A shutdown ends a job as `cancelled(shutdown)`.
//! * **Bounded.** The core holds at most [`MAX_EVALUATION_JOBS`] jobs, finished
//!   ones included (none is forgotten, so a resent id never runs twice); beyond
//!   that a new id is refused with `limit_exceeded` until the core restarts.
//!
//! The core reads the evaluation files; a view carries no path and no raw file
//! error. A file that is simply not there is [`ArtifactPresence::Missing`], one
//! that is there and cannot be used is [`ArtifactPresence::Problem`] with its
//! [`EvaluationProblemKind`]; the two are never reported as one another.
use super::diagnostics::{BoundedText, OperationTarget, ReadOutcome};
use crate::{
    errors::ErrorCode,
    handshake::{Capability, WireLimits},
    ids::{EvaluationGroupId, EvaluationMemberId, OperationId, ThreadId},
    limits::{MAX_INLINE_MESSAGE_BYTES, MAX_NAME_BYTES, MAX_TITLE_BYTES},
    management::{ArtifactPresence, EvaluationArtifactRef, EvaluationJobKind},
    operation::{OperationState, OperationView},
    queries::PageRequest,
    validation::{ValidationError, check_bytes, check_count, check_line, check_text},
    views::Page,
    wire::Revision,
};
use serde::{Deserialize, Serialize};

pub const COMMAND_OPERATIONS: &[&str] = &[
    "start_evaluation_run",
    "start_evaluation_report",
    "cancel_evaluation_job",
];
pub const QUERY_OPERATIONS: &[&str] = &[
    "list_evaluations",
    "get_evaluation",
    "get_evaluation_report",
    "get_evaluation_operation",
    "get_evaluation_job",
];
pub const EVENT_TYPES: &[&str] = &[
    "evaluation_operation_changed",
    "evaluation_job_progress",
    "evaluation_job_settled",
];
/// Tagged choices inside a command's payload that classification names (none).
pub const NESTED_COMMANDS: &[super::NestedOperation] = &[];

/// Most members one [`EvaluationGroupDetail`] lists.
pub const MAX_EVALUATION_MEMBERS: usize = 64;
/// Jobs the core holds (running and finished).
pub const MAX_EVALUATION_JOBS: usize = 256;
/// Largest `repeat` of a run.
pub const MAX_EVALUATION_REPEAT: u32 = 64;
/// Largest overlay text of a run, in bytes.
pub const MAX_CANDIDATE_BYTES: usize = 64 * 1024;
/// Longest `verify` command of a run, in bytes.
pub const MAX_VERIFY_BYTES: usize = 4 * 1024;
/// Largest `verify_timeout_secs` of a run (a day).
pub const MAX_VERIFY_TIMEOUT_SECS: u32 = 86_400;
/// Most directories one report job names.
pub const MAX_REPORT_TARGETS: usize = 16;
/// Most replay lines one [`EvaluationJobView`] lists.
pub const MAX_JOB_RUNS: usize = 128;
/// Most workspace-cleanup warnings one view lists.
pub const MAX_JOB_WARNINGS: usize = 8;
/// Longest [`EvaluationJobView::message`], and each warning, in bytes.
pub const MAX_JOB_MESSAGE_BYTES: usize = 512;

/// A directory under the evaluation root, by name: one plain component of
/// ASCII letters, digits, `.`, `_` and `-` that does not start with a dot (so
/// neither `..` nor the group records of the root).
///
/// # Errors
/// The name is empty, too long or has another character or a leading dot.
pub fn validate_target_name(field: &'static str, name: &str) -> Result<(), ValidationError> {
    check_bytes(field, name, MAX_NAME_BYTES, false)?;
    if name.starts_with('.')
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    {
        return Err(ValidationError::InvalidChars { field });
    }
    Ok(())
}

/// Replay a recorded thread under the user's configuration, and again with an
/// overlay (the candidate) merged over it; then report the directory unless
/// `report` is false.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartEvaluationRun {
    pub operation_id: OperationId,
    pub thread: ThreadId,
    /// The text of the candidate overlay (a configuration fragment).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate: Option<String>,
    /// Replays per configuration; the configured `eval.repeat_target` when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repeat: Option<u32>,
    /// The output directory, by name under the evaluation root (it must be
    /// missing or empty); a name from the time and the thread when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    /// What went wrong in the thread; judged as an aspect of its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
    /// A command run in each replay's final workspace, without network.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verify: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verify_timeout_secs: Option<u32>,
    #[serde(default)]
    pub keep_workspace: bool,
    /// Report the directory when the replays are done.
    pub report: bool,
}

impl StartEvaluationRun {
    pub fn validate(&self) -> Result<(), ValidationError> {
        if let Some(candidate) = &self.candidate {
            check_text("candidate", candidate, MAX_CANDIDATE_BYTES, false)?;
        }
        if let Some(repeat) = self.repeat
            && !(1..=MAX_EVALUATION_REPEAT).contains(&repeat)
        {
            return Err(ValidationError::OutOfRange {
                field: "repeat",
                reason: "must be 1..=64",
            });
        }
        if let Some(output) = &self.output {
            validate_target_name("output", output)?;
        }
        if let Some(problem) = &self.problem {
            check_text("problem", problem, MAX_INLINE_MESSAGE_BYTES, false)?;
        }
        if let Some(verify) = &self.verify {
            check_text("verify", verify, MAX_VERIFY_BYTES, false)?;
        }
        if let Some(secs) = self.verify_timeout_secs
            && !(1..=MAX_VERIFY_TIMEOUT_SECS).contains(&secs)
        {
            return Err(ValidationError::OutOfRange {
                field: "verify_timeout_secs",
                reason: "must be 1..=86400",
            });
        }
        Ok(())
    }
}

/// Report evaluation directories, or judge them again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartEvaluationReport {
    pub operation_id: OperationId,
    /// Directories by name under the evaluation root; several are also
    /// tabulated side by side.
    pub targets: Vec<String>,
    /// Judge again even where a judgment is cached (kind `rejudge`).
    #[serde(default)]
    pub rejudge: bool,
    /// Only grade the runs against the sample's checks, from their recorded
    /// evidence: nothing is judged, simulated, verified or priced. Cannot be
    /// combined with `rejudge`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub checks_only: bool,
}

impl StartEvaluationReport {
    pub fn kind(&self) -> EvaluationJobKind {
        if self.rejudge {
            EvaluationJobKind::Rejudge
        } else {
            EvaluationJobKind::GenerateReport
        }
    }

    pub fn validate(&self) -> Result<(), ValidationError> {
        if self.targets.is_empty() {
            return Err(ValidationError::Empty { field: "targets" });
        }
        check_count("targets", self.targets.len(), MAX_REPORT_TARGETS)?;
        for target in &self.targets {
            validate_target_name("targets", target)?;
        }
        if self.checks_only && self.rejudge {
            return Err(ValidationError::Inconsistent {
                field: "checks_only",
                reason: "a checks-only report judges nothing, so it cannot rejudge",
            });
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CancelEvaluationJob {
    /// The job to cancel (the id its start named).
    pub operation_id: OperationId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum EvaluationCommand {
    StartEvaluationRun(Box<StartEvaluationRun>),
    StartEvaluationReport(StartEvaluationReport),
    CancelEvaluationJob(CancelEvaluationJob),
}

impl EvaluationCommand {
    pub fn required_capability(&self) -> Capability {
        Capability::Commands
    }

    pub fn validate(&self) -> Result<(), ValidationError> {
        match self {
            Self::StartEvaluationRun(run) => run.validate(),
            Self::StartEvaluationReport(report) => report.validate(),
            Self::CancelEvaluationJob(_) => Ok(()),
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
pub enum EvaluationResult {
    /// The job, pending or settled.
    Job(Box<EvaluationJobView>),
}

/// How a replay ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvaluationRunStatus {
    Completed,
    Incomplete,
    Failed,
    PrepareFailed,
}

/// One replay that ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluationRunLine {
    /// The configuration, `baseline` or `candidate`.
    pub label: String,
    pub index: u32,
    pub status: EvaluationRunStatus,
    pub reason: String,
    pub user_turns: u32,
    pub tokens: u64,
    pub tool_calls: u32,
    pub elapsed_ms: u64,
    pub denied_requests: u32,
    pub pushed_refs: u32,
    /// Whether the verify command passed; absent when none ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verification_passed: Option<bool>,
}

impl EvaluationRunLine {
    pub fn validate(&self) -> Result<(), ValidationError> {
        check_line("label", &self.label, MAX_NAME_BYTES, false)?;
        check_bytes("reason", &self.reason, MAX_TITLE_BYTES, true)
    }
}

/// What a job left on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvaluationJobEffect {
    /// Nothing was written.
    None,
    /// The job failed or was cancelled after it began its output directory:
    /// the directory is neither untouched nor finished.
    Partial,
    /// The job ended `ready`: it did what it was asked.
    Complete,
    /// The job's task broke off and does not say what it wrote. Look at the
    /// directory before running the job again.
    Unknown,
}

/// What kind of failure ended a job (its state carries the error code).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvaluationJobFailure {
    /// The request does not fit the configuration or the files it names.
    Usage,
    /// The thread cannot be replayed, or its record cannot be read.
    Source,
    /// The candidate overlay is unusable.
    Overlay,
    /// The preflight failed; its evidence stays in the output directory.
    Preflight,
    /// The replays ended and the report could not be made.
    Report,
    Io,
    Internal,
}

/// A job and how it stands. The state of `operation` carries the error code;
/// `message` is a bounded, secret-free sentence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluationJobView {
    pub operation: OperationView,
    pub kind: EvaluationJobKind,
    /// The replays that ended, in order (at most [`MAX_JOB_RUNS`]).
    pub runs: Vec<EvaluationRunLine>,
    /// Replays that ended beyond the lines kept.
    #[serde(default)]
    pub runs_omitted: u32,
    /// The output directory of a run job, by name under the evaluation root;
    /// present once the job knows it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    /// The directories whose `report.md` the job wrote, by name.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reports: Vec<String>,
    /// The side-by-side table of several reports.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cross_thread: Option<BoundedText>,
    pub effect: EvaluationJobEffect,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<EvaluationJobFailure>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Workspaces that were not removed (retry with `evorch eval clean`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

impl EvaluationJobView {
    pub fn validate(&self) -> Result<(), ValidationError> {
        self.operation.validate()?;
        check_count("runs", self.runs.len(), MAX_JOB_RUNS)?;
        for run in &self.runs {
            run.validate()?;
        }
        check_count("reports", self.reports.len(), MAX_REPORT_TARGETS)?;
        for name in self.output.iter().chain(&self.reports) {
            validate_target_name("output", name)?;
        }
        if let Some(table) = &self.cross_thread {
            table.validate()?;
        }
        if let Some(message) = &self.message {
            check_bytes("message", message, MAX_JOB_MESSAGE_BYTES, true)?;
        }
        check_count("warnings", self.warnings.len(), MAX_JOB_WARNINGS)?;
        for warning in &self.warnings {
            check_bytes("warnings", warning, MAX_JOB_MESSAGE_BYTES, true)?;
        }
        let ready = self.operation.state == OperationState::Ready;
        if ready != (self.effect == EvaluationJobEffect::Complete) {
            return Err(ValidationError::Inconsistent {
                field: "effect",
                reason: "a job is complete exactly when it is ready",
            });
        }
        if self.effect == EvaluationJobEffect::Unknown
            && !matches!(self.operation.state, OperationState::Failed { .. })
        {
            return Err(ValidationError::Inconsistent {
                field: "effect",
                reason: "only a failed job can leave an unknown effect",
            });
        }
        if self.failure.is_some() && !matches!(self.operation.state, OperationState::Failed { .. })
        {
            return Err(ValidationError::Inconsistent {
                field: "failure",
                reason: "only a failed job names a failure",
            });
        }
        Ok(())
    }

    /// The error code when the job failed.
    pub fn failure_code(&self) -> Option<ErrorCode> {
        match self.operation.state {
            OperationState::Failed { code } => Some(code),
            _ => None,
        }
    }
}

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
    /// One run, report or rejudge job, whatever its state.
    GetEvaluationJob(OperationTarget),
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
            | Self::GetEvaluationOperation(_)
            | Self::GetEvaluationJob(_) => Ok(()),
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
    Job(Box<EvaluationJobView>),
}

impl EvaluationReply {
    pub fn validate(&self, _limits: &WireLimits) -> Result<(), ValidationError> {
        match self {
            Self::List(outcome) => outcome
                .validate_with(|list| list.groups.validate_with(EvaluationGroupSummary::validate)),
            Self::Group(outcome) => outcome.validate_with(EvaluationGroupDetail::validate),
            Self::Report(outcome) => outcome.validate_with(EvaluationReportText::validate),
            Self::Operation(view) => view.validate(),
            Self::Job(view) => view.validate(),
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
    /// A replay of a running job ended.
    EvaluationJobProgress { view: Box<EvaluationJobView> },
    /// The job reached its final state.
    EvaluationJobSettled { view: Box<EvaluationJobView> },
}

impl EvaluationEvent {
    pub fn validate(&self) -> Result<(), ValidationError> {
        match self {
            Self::EvaluationOperationChanged { view } => view.validate(),
            Self::EvaluationJobProgress { view } => {
                view.validate()?;
                if view.operation.state.is_final() {
                    return Err(ValidationError::Inconsistent {
                        field: "view",
                        reason: "a progress event names a running job",
                    });
                }
                Ok(())
            }
            Self::EvaluationJobSettled { view } => {
                view.validate()?;
                if !view.operation.state.is_final() {
                    return Err(ValidationError::Inconsistent {
                        field: "view",
                        reason: "a settled event names a final job",
                    });
                }
                Ok(())
            }
        }
    }
}
