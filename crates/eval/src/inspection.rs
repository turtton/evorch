//! Read-only inspection of the evaluations an Evaluator launched: a group, the
//! evaluation of each thread in it (a member) and each member's trials, as
//! typed snapshots that do not depend on any GUI.
//!
//! The reader only looks. It never starts, reruns or reports an evaluation,
//! calls a judge, prices a request against a provider or applies a
//! configuration. Every path is confined to the evaluation root, nothing is
//! followed through a symbolic link, and every read is bounded. A file that is
//! absent, still being replaced, too large or malformed shows up as a
//! problem of that artifact, and the rest of the snapshot still stands, so
//! polling a running evaluation is safe.
//!
//! The functions block on the file system; call them from a blocking task or a
//! background thread.
//!
//! The `eval_report` tool reads groups through this module, so the tool and a
//! GUI agree on what a state means. In particular a record without an exit is
//! "running" only for a process this one launched (see [`LaunchMonitor`]);
//! otherwise it is [`MemberPhase::Unverified`], not running, and a process id
//! is never taken as proof that a process lives.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::cost::{Cost, PriceTable, RequestUsage};
use crate::metrics;
use crate::output::{self, Manifest, Outcome, RunEntry};
use crate::report::Report;
use crate::runner::{BASELINE, CANDIDATE};

/// Return from the enclosing function unless the artifact is present.
macro_rules! present {
    ($artifact:expr) => {
        match $artifact {
            Artifact::Present(value) => value,
            Artifact::Missing => return Artifact::Missing,
            Artifact::Problem(problem) => return Artifact::Problem(problem),
        }
    };
}

mod artifact;
mod record;
mod snapshot;
mod state;
#[cfg(test)]
mod tests;

pub use artifact::{Artifact, ArtifactProblem, BoundedText, FileRef, FileState, ProblemKind};
pub(crate) use record::{
    GROUP_FILE, GROUP_SCHEMA, GROUPS_DIR, GroupRecord, Member, OVERLAY_FILE, lock,
};
pub use record::{LaunchMonitor, ProcessExit, ProcessState, Purpose};
pub use snapshot::*;

use artifact::{
    child_dir, file_ref, read_json, read_text_head, read_text_tail, read_versioned_json,
};
use state::Activity;

const STDERR_BYTES: u64 = 16 * 1024;
const STDERR_CHARS: usize = 1_200;
const REASON_CHARS: usize = 2_000;
const RUNS_DIR: &str = "runs";

/// Bounds on what one inspection reads.
#[derive(Debug, Clone)]
pub struct InspectionLimits {
    /// Groups a listing returns at most.
    pub max_groups: usize,
    /// Directory entries a listing looks at before it gives up counting.
    pub max_scan_entries: usize,
    /// Members read per group.
    pub max_members: usize,
    /// Trials listed per member.
    pub max_trials: usize,
    /// Size of a JSON file the reader parses.
    pub max_json_bytes: u64,
    /// Bytes of `report.md` one read returns at most.
    pub max_markdown_bytes: u64,
    /// Bytes of the candidate overlay kept in a snapshot.
    pub max_overlay_bytes: u64,
}

impl Default for InspectionLimits {
    fn default() -> Self {
        Self {
            max_groups: 50,
            max_scan_entries: 10_000,
            max_members: 64,
            max_trials: 256,
            max_json_bytes: 8 * 1024 * 1024,
            max_markdown_bytes: 1024 * 1024,
            max_overlay_bytes: 4 * 1024,
        }
    }
}

/// A group with the reports its members left, for callers that need more than
/// the snapshot carries.
pub(crate) struct Inspected {
    pub snapshot: EvaluationGroupSnapshot,
    /// In member order; `None` where a member has no usable report.
    pub reports: Vec<Option<Report>>,
}

/// Reads the evaluations under `<user config dir>/eval`.
#[derive(Clone)]
pub struct EvaluationInspector {
    root: PathBuf,
    monitor: Option<LaunchMonitor>,
    prices: Option<Arc<PriceTable>>,
    limits: InspectionLimits,
}

impl EvaluationInspector {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            monitor: None,
            prices: None,
            limits: InspectionLimits::default(),
        }
    }

    /// Know which processes this process launched and when they ended.
    #[must_use]
    pub fn with_monitor(mut self, monitor: LaunchMonitor) -> Self {
        self.monitor = Some(monitor);
        self
    }

    /// Estimate the cost of finished trials from their own databases while
    /// the report is not there. Without prices such a cost is unavailable;
    /// it is never zero.
    #[must_use]
    pub fn with_prices(mut self, prices: Arc<PriceTable>) -> Self {
        self.prices = Some(prices);
        self
    }

    #[must_use]
    pub fn with_limits(mut self, limits: InspectionLimits) -> Self {
        self.limits = limits;
        self
    }

    /// The newest groups first, at most `limit` (and the configured maximum).
    /// Costs are not estimated here; read one group for those.
    pub fn list_groups(&self, limit: usize) -> GroupListing {
        let empty = GroupListing {
            entries: Vec::new(),
            omitted: 0,
            scan_truncated: false,
            problem: None,
        };
        let Ok(root) = self.root.canonicalize() else {
            return empty;
        };
        let groups = match child_dir(&root, GROUPS_DIR) {
            Artifact::Present(groups) => groups,
            Artifact::Missing => return empty,
            Artifact::Problem(problem) => {
                return GroupListing {
                    problem: Some(problem),
                    ..empty
                };
            }
        };
        let entries = match std::fs::read_dir(&groups) {
            Ok(entries) => entries,
            Err(error) => {
                return GroupListing {
                    problem: Some(ArtifactProblem::io(&groups, &error)),
                    ..empty
                };
            }
        };
        let mut ids = Vec::new();
        let mut scan_truncated = false;
        for (scanned, entry) in entries.filter_map(Result::ok).enumerate() {
            if scanned == self.limits.max_scan_entries {
                scan_truncated = true;
                break;
            }
            if let Ok(name) = entry.file_name().into_string()
                && is_group_id(&name)
            {
                ids.push(name);
            }
        }
        ids.sort();
        ids.reverse();
        let shown = limit.min(self.limits.max_groups);
        let omitted = ids.len().saturating_sub(shown);
        ids.truncate(shown);
        GroupListing {
            entries: ids
                .into_iter()
                .map(|id| GroupListEntry {
                    group: self
                        .inspect_group(&id, false)
                        .map(|inspected| inspected.snapshot),
                    id,
                })
                .collect(),
            omitted,
            scan_truncated,
            problem: None,
        }
    }

    /// One group with its members and trials, costs included when prices were
    /// given.
    pub fn group(&self, id: &str) -> Artifact<EvaluationGroupSnapshot> {
        self.inspect_group(id, true)
            .map(|inspected| inspected.snapshot)
    }

    /// A member's whole `report.json`.
    pub fn member_report(&self, group_id: &str, member: usize) -> Artifact<Report> {
        let dir = present!(self.member_location(group_id, member));
        read_json(
            &dir.join(output::REPORT_JSON_FILE),
            self.limits.max_json_bytes,
        )
    }

    /// The start of a member's `report.md`, at most `max_bytes` (and the
    /// configured maximum).
    pub fn member_report_markdown(
        &self,
        group_id: &str,
        member: usize,
        max_bytes: u64,
    ) -> Artifact<BoundedText> {
        let dir = present!(self.member_location(group_id, member));
        read_text_head(
            &dir.join(output::REPORT_MARKDOWN_FILE),
            max_bytes.min(self.limits.max_markdown_bytes),
        )
    }

    fn canonical_root(&self) -> Artifact<PathBuf> {
        match self.root.canonicalize() {
            Ok(root) => Artifact::Present(root),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Artifact::Missing,
            Err(error) => Artifact::Problem(ArtifactProblem::io(&self.root, &error)),
        }
    }

    fn member_location(&self, group_id: &str, member: usize) -> Artifact<PathBuf> {
        let root = present!(self.canonical_root());
        let group_dir = present!(group_dir(&root, group_id));
        let record = present!(self.load_record(&group_dir, group_id));
        match record.members.get(member) {
            Some(member) => member_dir(&root, member),
            None => Artifact::Missing,
        }
    }

    fn load_record(&self, group_dir: &Path, id: &str) -> Artifact<GroupRecord> {
        let path = group_dir.join(GROUP_FILE);
        let record: GroupRecord = present!(read_versioned_json(
            &path,
            self.limits.max_json_bytes,
            GROUP_SCHEMA
        ));
        if record.id != id {
            return Artifact::Problem(ArtifactProblem::new(
                ProblemKind::Malformed,
                &path,
                format!("belongs to group {:?}, not {id:?}", record.id),
            ));
        }
        Artifact::Present(record)
    }

    pub(crate) fn inspect_group(&self, id: &str, with_costs: bool) -> Artifact<Inspected> {
        let root = present!(self.canonical_root());
        let group_dir = present!(group_dir(&root, id));
        let record = present!(self.load_record(&group_dir, id));
        let launched = self
            .monitor
            .as_ref()
            .and_then(|monitor| monitor.processes(id))
            .filter(|processes| processes.len() == record.members.len());
        let mut members = Vec::new();
        let mut reports = Vec::new();
        for (index, member) in record
            .members
            .iter()
            .enumerate()
            .take(self.limits.max_members)
        {
            let process = match launched.as_ref().and_then(|processes| processes.get(index)) {
                Some(ProcessState::Running) => ProcessStatus::Running,
                Some(ProcessState::Exited(exit)) => ProcessStatus::Exited(exit.clone()),
                None => member
                    .exit
                    .clone()
                    .map_or(ProcessStatus::Unknown, ProcessStatus::Exited),
            };
            let (snapshot, report) = self.inspect_member(&MemberInput {
                root: &root,
                group_dir: &group_dir,
                index,
                member,
                process,
                with_costs,
            });
            members.push(snapshot);
            reports.push(report);
        }
        let phases: Vec<MemberPhase> = members.iter().map(|member| member.phase).collect();
        let exits: Option<Vec<&str>> = record
            .members
            .iter()
            .enumerate()
            .map(
                |(index, member)| match members.get(index).map(|m| &m.process) {
                    Some(ProcessStatus::Exited(exit)) => Some(exit.finished_at.as_str()),
                    Some(_) => None,
                    None => member.exit.as_ref().map(|exit| exit.finished_at.as_str()),
                },
            )
            .collect();
        // The record on disk can lag the process that waited for the last exit.
        let finished_at = record.finished_at.clone().or_else(|| {
            exits
                .filter(|exits| !exits.is_empty())
                .and_then(|exits| exits.into_iter().max().map(str::to_owned))
        });
        Artifact::Present(Inspected {
            snapshot: EvaluationGroupSnapshot {
                id: record.id.clone(),
                run_id: record.run_id.clone(),
                started_at: record.started_at.clone(),
                finished_at,
                notice: record.notice.clone(),
                phase: state::group_phase(&phases),
                candidate: read_text_head(
                    &group_dir.join(OVERLAY_FILE),
                    self.limits.max_overlay_bytes,
                ),
                members,
                members_omitted: record.members.len().saturating_sub(self.limits.max_members),
            },
            reports,
        })
    }

    fn inspect_member(
        &self,
        input: &MemberInput<'_>,
    ) -> (EvaluationMemberSnapshot, Option<Report>) {
        let MemberInput {
            root,
            group_dir,
            index,
            member,
            process,
            with_costs,
        } = input;
        let limit = self.limits.max_json_bytes;
        let dir = member_dir(root, member);
        let mut problems = Vec::new();
        let (manifest, report) = match &dir {
            Artifact::Present(dir) => (
                read_json::<Manifest>(&dir.join(output::MANIFEST_FILE), limit),
                read_json::<Report>(&dir.join(output::REPORT_JSON_FILE), limit),
            ),
            Artifact::Missing => (Artifact::Missing, Artifact::Missing),
            Artifact::Problem(problem) => {
                problems.push(problem.clone());
                (Artifact::Missing, Artifact::Missing)
            }
        };
        if let Some(problem) = manifest.problem() {
            problems.push(problem.clone());
        }
        let manifest_finished = manifest.present().map(|m| m.finished_at.is_some());
        let report_available = report.present().is_some();
        let activity = state::activity(process, manifest_finished == Some(true), report_available);
        let phase = state::member_phase(process, manifest_finished, report_available);

        let (report_status, full_report) = match report {
            Artifact::Present(report) => (
                ReportStatus::Available(Box::new(ReportSummary {
                    generated_at: report.generated_at.clone(),
                    pairs_judged: report.pairs.len(),
                    tally: report.tally.clone(),
                    costs: report.costs.clone(),
                    warnings: report.warnings.len(),
                })),
                Some(report),
            ),
            Artifact::Missing if *process == ProcessStatus::Running => {
                (ReportStatus::Pending, None)
            }
            Artifact::Missing => (ReportStatus::Missing, None),
            Artifact::Problem(problem) => (ReportStatus::Unreadable(problem), None),
        };

        let plan = Plan::of(manifest.present(), member.repeat);
        let runs: &[RunEntry] = manifest.present().map_or(&[], |m| m.runs.as_slice());
        let specs = self.trial_specs(&plan, runs);
        let runs_dir = match &dir {
            Artifact::Present(dir) => child_dir(dir, RUNS_DIR),
            Artifact::Missing => Artifact::Missing,
            Artifact::Problem(problem) => Artifact::Problem(problem.clone()),
        };
        let trials: Vec<EvaluationTrialSnapshot> = specs
            .0
            .iter()
            .map(|(label, trial, entry)| {
                self.inspect_trial(&TrialInput {
                    label,
                    index: *trial,
                    entry: *entry,
                    runs_dir: &runs_dir,
                    activity,
                    manifest_finished: manifest_finished == Some(true),
                    report: full_report.as_ref(),
                    with_costs: *with_costs,
                })
            })
            .collect();

        let (failure, stderr_tail) = match process {
            ProcessStatus::Exited(exit) if !exit.succeeded() && !report_available => {
                let tail = stderr_tail(group_dir, &member.stderr, &mut problems);
                (Some(exit.describe()), Some(tail))
            }
            _ => (None, None),
        };
        let progress = manifest.present().map(|manifest| TrialProgress {
            planned: manifest
                .configurations
                .len()
                .saturating_mul(manifest.repeat as usize),
            started: manifest.runs.len(),
            finished: manifest
                .runs
                .iter()
                .filter(|run| run.finished_at.is_some())
                .count(),
            manifest_finished: manifest.finished_at.is_some(),
        });
        let snapshot = EvaluationMemberSnapshot {
            index: *index,
            thread_id: member.thread_id.clone(),
            purpose: member.purpose,
            repeat: member.repeat,
            dir_name: member
                .dir
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default(),
            report_markdown: dir
                .present()
                .map(|dir| file_ref(dir.join(output::REPORT_MARKDOWN_FILE))),
            dir,
            pid: member.pid,
            phase,
            process: process.clone(),
            started_at: manifest.present().map(|m| m.started_at.clone()),
            progress,
            cost: member_cost(full_report.as_ref(), &trials),
            trials,
            trials_omitted: specs.1,
            report: report_status,
            failure,
            stderr_tail,
            problems,
        };
        (snapshot, full_report)
    }

    /// The trials to list, planned ones first in replay order, then any the
    /// manifest holds beyond the plan, and how many were left out.
    fn trial_specs<'a>(&self, plan: &'a Plan, runs: &'a [RunEntry]) -> (Vec<TrialSpec<'a>>, usize) {
        let max = self.limits.max_trials;
        let mut specs = Vec::new();
        'plan: for label in &plan.labels {
            for trial in 1..=plan.repeat {
                if specs.len() >= max {
                    break 'plan;
                }
                let entry = runs
                    .iter()
                    .rev()
                    .find(|entry| entry.label == *label && entry.index == trial);
                specs.push((label.as_str(), trial, entry));
            }
        }
        let extra = |entry: &&RunEntry| !plan.contains(&entry.label, entry.index);
        let extras = runs.iter().filter(extra).count();
        for entry in runs.iter().filter(extra) {
            if specs.len() >= max {
                break;
            }
            specs.push((entry.label.as_str(), entry.index, Some(entry)));
        }
        let omitted = plan
            .size()
            .saturating_add(extras)
            .saturating_sub(specs.len());
        (specs, omitted)
    }

    fn inspect_trial(&self, input: &TrialInput<'_>) -> EvaluationTrialSnapshot {
        let TrialInput {
            label,
            index,
            entry,
            runs_dir,
            activity,
            manifest_finished,
            report,
            with_costs,
        } = input;
        let state = state::trial_state(*entry, *activity, *manifest_finished);
        let ended = matches!(state, TrialState::Finished | TrialState::Failed);
        let name = format!("{label}-{index}");
        let trial_dir = entry.map(|_| match runs_dir {
            Artifact::Present(runs) => child_dir(runs, &name),
            Artifact::Missing => Artifact::Missing,
            Artifact::Problem(problem) => Artifact::Problem(problem.clone()),
        });
        let outcome = match &trial_dir {
            Some(Artifact::Present(dir)) if ended => {
                read_json::<Outcome>(&dir.join(output::OUTCOME_FILE), self.limits.max_json_bytes)
                    .map(|outcome| outcome_of(&outcome))
            }
            Some(Artifact::Problem(problem)) if ended => Artifact::Problem(problem.clone()),
            _ => Artifact::Missing,
        };
        let reported = report.and_then(|report| {
            report
                .runs
                .iter()
                .find(|run| run.label == *label && run.index == *index)
        });
        let cost = if let Some(run) = reported {
            TrialCost::Reported(run.cost.clone())
        } else if !ended {
            TrialCost::Unavailable(CostUnavailable::NotFinished)
        } else if report.is_some() {
            TrialCost::Unavailable(CostUnavailable::NotInReport)
        } else {
            match (&self.prices, &trial_dir, with_costs) {
                (Some(prices), Some(Artifact::Present(dir)), true) => estimate_cost(dir, prices),
                (Some(_), Some(Artifact::Missing), true) => {
                    TrialCost::Unavailable(CostUnavailable::NoDatabase)
                }
                (Some(_), Some(Artifact::Problem(problem)), true) => {
                    TrialCost::Unavailable(CostUnavailable::Unreadable(problem.clone()))
                }
                _ => TrialCost::Unavailable(CostUnavailable::NotPriced),
            }
        };
        EvaluationTrialSnapshot {
            label: (*label).to_owned(),
            index: *index,
            state,
            started_at: entry.map(|entry| entry.started_at.clone()),
            finished_at: entry.and_then(|entry| entry.finished_at.clone()),
            status: entry.and_then(|entry| entry.status),
            outcome,
            cost,
            artifacts: trial_dir.map(|dir| {
                dir.map(|dir| TrialArtifacts {
                    database: file_ref(dir.join(output::DATABASE_FILE)),
                    diff: file_ref(dir.join(output::DIFF_FILE)),
                    conversation: file_ref(dir.join(output::CONVERSATION_FILE)),
                    outcome: file_ref(dir.join(output::OUTCOME_FILE)),
                    dir,
                })
            }),
        }
    }
}

/// A trial to list: its label, its repeat number and the manifest's entry for it.
type TrialSpec<'a> = (&'a str, u32, Option<&'a RunEntry>);

struct MemberInput<'a> {
    root: &'a Path,
    group_dir: &'a Path,
    index: usize,
    member: &'a Member,
    process: ProcessStatus,
    with_costs: bool,
}

struct TrialInput<'a> {
    label: &'a str,
    index: u32,
    entry: Option<&'a RunEntry>,
    runs_dir: &'a Artifact<PathBuf>,
    activity: Activity,
    manifest_finished: bool,
    report: Option<&'a Report>,
    with_costs: bool,
}

/// The trials a member is to run: its configurations, each repeated.
struct Plan {
    labels: Vec<String>,
    repeat: u32,
}

impl Plan {
    /// A launched group always has a candidate, so before the manifest names
    /// the configurations both are expected.
    fn of(manifest: Option<&Manifest>, launched_repeat: u32) -> Self {
        manifest.map_or_else(
            || Self {
                labels: vec![BASELINE.to_owned(), CANDIDATE.to_owned()],
                repeat: launched_repeat,
            },
            |manifest| Self {
                labels: manifest.configurations.clone(),
                repeat: manifest.repeat,
            },
        )
    }

    fn size(&self) -> usize {
        self.labels.len().saturating_mul(self.repeat as usize)
    }

    fn contains(&self, label: &str, index: u32) -> bool {
        self.labels.iter().any(|known| known == label) && (1..=self.repeat).contains(&index)
    }
}

fn is_group_id(id: &str) -> bool {
    !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

fn group_dir(root: &Path, id: &str) -> Artifact<PathBuf> {
    if !is_group_id(id) {
        return Artifact::Problem(ArtifactProblem::new(
            ProblemKind::UnsafePath,
            root,
            format!("{id:?} is not an evaluation group id"),
        ));
    }
    let groups = present!(child_dir(root, GROUPS_DIR));
    child_dir(&groups, id)
}

/// A member's output directory: directly under the root, named as recorded,
/// and not a link.
fn member_dir(root: &Path, member: &Member) -> Artifact<PathBuf> {
    let unsafe_path = |detail: String| {
        Artifact::Problem(ArtifactProblem::new(
            ProblemKind::UnsafePath,
            &member.dir,
            detail,
        ))
    };
    let Some(name) = member.dir.file_name().and_then(OsStr::to_str) else {
        return unsafe_path("has no usable name".into());
    };
    if name.starts_with('.') {
        return unsafe_path("is a hidden directory".into());
    }
    match member.dir.parent().map(Path::canonicalize) {
        Some(Ok(parent)) if parent == root => child_dir(root, name),
        _ => unsafe_path(format!(
            "is not directly under the evaluation root {}",
            root.display()
        )),
    }
}

fn outcome_of(outcome: &Outcome) -> TrialOutcome {
    TrialOutcome {
        status: outcome.status,
        reason: metrics::clip(&outcome.reason, REASON_CHARS),
        user_turns: outcome.user_turns,
        tokens: outcome.tokens,
        tool_calls: outcome.tool_calls,
        elapsed_ms: outcome.elapsed_ms,
        denied_requests: outcome.denied_requests.len(),
        pushed_refs: outcome.pushed_refs.len(),
        verification_passed: outcome.verification.as_ref().map(|v| v.passed),
    }
}

/// Reprice a finished trial's own requests. Reads the trial's database through
/// a read-only connection.
fn estimate_cost(dir: &Path, prices: &PriceTable) -> TrialCost {
    let database = dir.join(output::DATABASE_FILE);
    match std::fs::symlink_metadata(&database) {
        Ok(meta) if meta.is_file() => {}
        Ok(_) => {
            return TrialCost::Unavailable(CostUnavailable::Unreadable(ArtifactProblem::new(
                ProblemKind::UnsafePath,
                &database,
                "is not a regular file",
            )));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return TrialCost::Unavailable(CostUnavailable::NoDatabase);
        }
        Err(error) => {
            return TrialCost::Unavailable(CostUnavailable::Unreadable(ArtifactProblem::io(
                &database, &error,
            )));
        }
    }
    let rows = storage::Database::open_read_only(&database)
        .and_then(|opened| opened.usage_requests_between(i64::MIN, i64::MAX));
    match rows {
        Ok(rows) => {
            let requests: Vec<RequestUsage> = rows
                .iter()
                .map(|row| RequestUsage::from_record(&row.record))
                .collect();
            TrialCost::Estimated(prices.cost(requests.iter().filter(|r| !r.is_overhead())))
        }
        Err(error) => TrialCost::Unavailable(CostUnavailable::Unreadable(ArtifactProblem::new(
            ProblemKind::Io,
            &database,
            error.to_string(),
        ))),
    }
}

fn member_cost(report: Option<&Report>, trials: &[EvaluationTrialSnapshot]) -> MemberCost {
    if let Some(report) = report {
        return MemberCost {
            source: CostSource::Report,
            configurations: report
                .costs
                .iter()
                .map(|cost| ConfigurationCostSnapshot {
                    label: cost.label.clone(),
                    counted: cost.runs,
                    planned: cost.runs,
                    cost: cost.cost.clone(),
                })
                .collect(),
        };
    }
    let mut configurations: Vec<ConfigurationCostSnapshot> = Vec::new();
    for trial in trials {
        let position = configurations
            .iter()
            .position(|known| known.label == trial.label);
        let slot = match position {
            Some(position) => &mut configurations[position],
            None => {
                configurations.push(ConfigurationCostSnapshot {
                    label: trial.label.clone(),
                    counted: 0,
                    planned: 0,
                    cost: Cost::default(),
                });
                configurations.last_mut().expect("just pushed")
            }
        };
        slot.planned += 1;
        if let TrialCost::Estimated(cost) = &trial.cost {
            slot.counted += 1;
            slot.cost.add(cost);
        }
    }
    configurations.retain(|configuration| configuration.counted > 0);
    MemberCost {
        source: if configurations.is_empty() {
            CostSource::None
        } else {
            CostSource::Estimate
        },
        configurations,
    }
}

/// The end of the process's stderr, found beside the group record by the name
/// the record gives, never by the path it gives.
fn stderr_tail(group_dir: &Path, recorded: &Path, problems: &mut Vec<ArtifactProblem>) -> String {
    let Some(name) = recorded.file_name().and_then(OsStr::to_str) else {
        return String::new();
    };
    if !artifact::is_plain_name(name) {
        return String::new();
    }
    match read_text_tail(&group_dir.join(name), STDERR_BYTES) {
        Artifact::Present(text) => {
            let text = text.trim();
            let skip = text.chars().count().saturating_sub(STDERR_CHARS);
            text.chars().skip(skip).collect()
        }
        Artifact::Missing => String::new(),
        Artifact::Problem(problem) => {
            problems.push(problem);
            String::new()
        }
    }
}
