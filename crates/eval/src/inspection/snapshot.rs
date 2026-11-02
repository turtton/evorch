//! What an inspection reports: a group, its members (one per thread) and
//! their trials, as typed values a GUI or a tool can show without parsing
//! files itself.

use std::path::PathBuf;

use crate::cost::Cost;
use crate::driver::Status;
use crate::report::{AspectTally, ConfigurationCost};

use super::artifact::{Artifact, ArtifactProblem, BoundedText, FileRef};
use super::record::{ProcessExit, Purpose};

/// Where the group as a whole stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupPhase {
    /// A member is starting, replaying or reporting.
    Running,
    /// Every member has its report.
    Finished,
    /// Every member has ended, and some failed or has no report.
    Incomplete,
    /// No member is known to run, and the end of some was never seen.
    Unverified,
}

/// Where one member (the evaluation of one thread) stands. Process end, trial
/// end and the report are separate facts; each phase says which are known.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemberPhase {
    /// The process runs and has written no manifest yet.
    Starting,
    /// The process runs and has trials left.
    RunningTrials,
    /// Every trial ended and the process still runs: the report is being made.
    Reporting,
    /// The report exists and no failure was seen.
    Finished,
    /// The process ended successfully and left no usable report.
    FinishedWithoutReport,
    /// The process could not start, or ended with an error or a signal.
    Failed,
    /// Nothing says the process runs or how it ended: it was launched by
    /// another process, or an earlier session of this one.
    Unverified,
}

impl MemberPhase {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::RunningTrials => "running_trials",
            Self::Reporting => "reporting",
            Self::Finished => "finished",
            Self::FinishedWithoutReport => "finished_without_report",
            Self::Failed => "failed",
            Self::Unverified => "unverified",
        }
    }

    /// The state `eval_report` has always shown.
    pub const fn legacy_state(self) -> &'static str {
        match self {
            Self::Starting | Self::RunningTrials | Self::Reporting => "running",
            Self::Finished | Self::FinishedWithoutReport => "finished",
            Self::Failed => "failed",
            Self::Unverified => "interrupted",
        }
    }

    pub const fn is_active(self) -> bool {
        matches!(self, Self::Starting | Self::RunningTrials | Self::Reporting)
    }
}

/// What is known of the evaluation process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessStatus {
    /// Launched by this process and not yet ended.
    Running,
    Exited(ProcessExit),
    /// No evidence either way. A pid alone is none: it can be reused.
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrialState {
    /// Planned, not started, and the process is known to run.
    Queued,
    Running,
    /// Ended with a completed or incomplete replay.
    Finished,
    /// Ended with a failed replay.
    Failed,
    /// Started, and the process ended before the trial did.
    Interrupted,
    /// Never started and never will: the process ended first.
    Unavailable,
    /// Not finished, and nothing says whether the process still works on it.
    Unverified,
}

impl TrialState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Finished => "finished",
            Self::Failed => "failed",
            Self::Interrupted => "interrupted",
            Self::Unavailable => "unavailable",
            Self::Unverified => "unverified",
        }
    }
}

/// What `outcome.json` says of a finished trial.
#[derive(Debug, Clone, PartialEq)]
pub struct TrialOutcome {
    pub status: Status,
    pub reason: String,
    pub user_turns: u32,
    pub tokens: u64,
    pub tool_calls: u32,
    pub elapsed_ms: u64,
    pub denied_requests: usize,
    pub pushed_refs: usize,
    pub verification_passed: Option<bool>,
}

/// Why a trial has no cost.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CostUnavailable {
    /// The trial has not finished.
    NotFinished,
    /// Finished, but the report does not hold it and this inspection did not
    /// reprice (no price table was given, or it was a listing).
    NotPriced,
    /// The report exists and has no entry for the trial.
    NotInReport,
    /// The trial's database is absent.
    NoDatabase,
    Unreadable(ArtifactProblem),
}

/// A trial's cost. An unpriced or unreported request keeps the cost marked
/// incomplete (see [`Cost::is_complete`]); an unknown cost is never zero.
#[derive(Debug, Clone, PartialEq)]
pub enum TrialCost {
    /// From `report.json`.
    Reported(Cost),
    /// Repriced from the trial's own database before the report existed.
    Estimated(Cost),
    Unavailable(CostUnavailable),
}

impl TrialCost {
    pub const fn cost(&self) -> Option<&Cost> {
        match self {
            Self::Reported(cost) | Self::Estimated(cost) => Some(cost),
            Self::Unavailable(_) => None,
        }
    }

    pub const fn has_cost(&self) -> bool {
        self.cost().is_some()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrialArtifacts {
    pub dir: PathBuf,
    pub database: FileRef,
    pub diff: FileRef,
    pub conversation: FileRef,
    pub outcome: FileRef,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EvaluationTrialSnapshot {
    /// `baseline` or `candidate`.
    pub label: String,
    /// 1-based repeat number.
    pub index: u32,
    pub state: TrialState,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    /// The manifest's status once the trial ended.
    pub status: Option<Status>,
    /// Read for finished trials only.
    pub outcome: Artifact<TrialOutcome>,
    pub cost: TrialCost,
    /// `None` when the trial directory is not confined to the evaluation or
    /// the trial has not started.
    pub artifacts: Option<Artifact<TrialArtifacts>>,
}

impl EvaluationTrialSnapshot {
    pub fn name(&self) -> String {
        format!("{}-{}", self.label, self.index)
    }
}

/// A report's headline, for lists. The whole report is read with
/// [`super::EvaluationInspector::member_report`].
#[derive(Debug, Clone, PartialEq)]
pub struct ReportSummary {
    pub generated_at: String,
    pub pairs_judged: usize,
    pub tally: Vec<AspectTally>,
    pub costs: Vec<ConfigurationCost>,
    pub warnings: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ReportStatus {
    Available(Box<ReportSummary>),
    /// Not there yet, and the process still runs.
    Pending,
    /// Not there, and nothing is making it.
    Missing,
    Unreadable(ArtifactProblem),
}

impl ReportStatus {
    pub const fn is_available(&self) -> bool {
        matches!(self, Self::Available(_))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CostSource {
    /// `report.json`.
    Report,
    /// The databases of the trials that finished.
    Estimate,
    /// No cost is known.
    None,
}

/// The cost of one configuration's trials of a member.
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigurationCostSnapshot {
    pub label: String,
    /// Trials whose cost is in `cost`.
    pub counted: u32,
    pub planned: u32,
    pub cost: Cost,
}

impl ConfigurationCostSnapshot {
    pub const fn is_partial(&self) -> bool {
        self.counted < self.planned
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct MemberCost {
    pub source: CostSource,
    /// Empty when `source` is [`CostSource::None`].
    pub configurations: Vec<ConfigurationCostSnapshot>,
}

/// How far the member's trials have come, from the manifest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrialProgress {
    pub planned: usize,
    pub started: usize,
    pub finished: usize,
    /// The manifest says every trial ended.
    pub manifest_finished: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EvaluationMemberSnapshot {
    /// Position in the group.
    pub index: usize,
    pub thread_id: String,
    pub purpose: Purpose,
    pub repeat: u32,
    /// The output directory's name under the evaluation root.
    pub dir_name: String,
    /// The output directory once it exists and is confined to the root.
    pub dir: Artifact<PathBuf>,
    /// Informational only; never used to decide the process runs.
    pub pid: Option<u32>,
    pub phase: MemberPhase,
    pub process: ProcessStatus,
    pub started_at: Option<String>,
    pub progress: Option<TrialProgress>,
    /// Planned first, in replay order, then any the manifest has beyond the plan.
    pub trials: Vec<EvaluationTrialSnapshot>,
    pub trials_omitted: usize,
    pub report: ReportStatus,
    pub report_markdown: Option<FileRef>,
    pub cost: MemberCost,
    /// Why the process failed.
    pub failure: Option<String>,
    /// The end of the process's stderr when it failed.
    pub stderr_tail: Option<String>,
    /// Artifacts that could not be used; the rest of the snapshot still stands.
    pub problems: Vec<ArtifactProblem>,
}

impl EvaluationMemberSnapshot {
    pub fn planned_trials(&self) -> usize {
        self.progress.map_or(self.trials.len(), |p| p.planned)
    }

    pub fn finished_trials(&self) -> usize {
        self.trials
            .iter()
            .filter(|trial| matches!(trial.state, TrialState::Finished | TrialState::Failed))
            .count()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct EvaluationGroupSnapshot {
    pub id: String,
    /// The Evaluator run that started the group.
    pub run_id: String,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub notice: Option<String>,
    pub phase: GroupPhase,
    /// The start of the candidate overlay.
    pub candidate: Artifact<BoundedText>,
    pub members: Vec<EvaluationMemberSnapshot>,
    pub members_omitted: usize,
}

/// One entry of a group listing, newest first.
#[derive(Debug, Clone, PartialEq)]
pub struct GroupListEntry {
    pub id: String,
    pub group: Artifact<EvaluationGroupSnapshot>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GroupListing {
    pub entries: Vec<GroupListEntry>,
    /// Groups on disk beyond those listed, up to the scan bound.
    pub omitted: usize,
    /// More entries exist than the scan bound read.
    pub scan_truncated: bool,
    /// Why the groups directory could not be listed.
    pub problem: Option<ArtifactProblem>,
}
