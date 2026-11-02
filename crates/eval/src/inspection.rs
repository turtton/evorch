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

use std::collections::HashSet;
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

use artifact::Dir;
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
    /// Size of a trial database that may be read to estimate its cost.
    pub max_database_bytes: u64,
    /// Usage rows read from one trial database to estimate its cost.
    pub max_database_rows: usize,
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
            max_database_bytes: 256 * 1024 * 1024,
            max_database_rows: 200_000,
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
        let failed = |problem| GroupListing {
            problem: Some(problem),
            ..GroupListing {
                entries: Vec::new(),
                omitted: 0,
                scan_truncated: false,
                problem: None,
            }
        };
        let root = match self.open_root() {
            Artifact::Present(root) => root,
            Artifact::Missing => return empty,
            Artifact::Problem(problem) => return failed(problem),
        };
        let groups = match root.open_dir(GROUPS_DIR) {
            Artifact::Present(groups) => groups,
            Artifact::Missing => return empty,
            Artifact::Problem(problem) => return failed(problem),
        };
        let (names, scan_truncated) = match groups.names(self.limits.max_scan_entries) {
            Artifact::Present(found) => found,
            Artifact::Missing => return empty,
            Artifact::Problem(problem) => return failed(problem),
        };
        let mut ids: Vec<String> = names.into_iter().filter(|name| is_group_id(name)).collect();
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
                        .inspect_in(&root, &id, false)
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
        dir.read_json(output::REPORT_JSON_FILE, self.limits.max_json_bytes)
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
        dir.read_text_head(
            output::REPORT_MARKDOWN_FILE,
            max_bytes.min(self.limits.max_markdown_bytes),
        )
    }

    /// The evaluation root, opened once; a root that does not exist is
    /// `Missing`, and one that cannot be resolved for any other reason is a
    /// problem, not an empty root.
    fn open_root(&self) -> Artifact<Dir> {
        match self.root.canonicalize() {
            Ok(root) => Dir::open_root(&root),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Artifact::Missing,
            Err(error) => Artifact::Problem(ArtifactProblem::io(&self.root, &error)),
        }
    }

    fn member_location(&self, group_id: &str, member: usize) -> Artifact<Dir> {
        let root = present!(self.open_root());
        let group_dir = present!(open_group_dir(&root, group_id));
        let record = present!(self.load_record(&group_dir, group_id));
        match record.members.get(member) {
            Some(member) => open_member_dir(&root, member),
            None => Artifact::Missing,
        }
    }

    fn load_record(&self, group_dir: &Dir, id: &str) -> Artifact<GroupRecord> {
        let record: GroupRecord = present!(group_dir.read_versioned_json(
            GROUP_FILE,
            self.limits.max_json_bytes,
            GROUP_SCHEMA
        ));
        if record.id != id {
            return Artifact::Problem(ArtifactProblem::new(
                ProblemKind::Malformed,
                &group_dir.join(GROUP_FILE),
                format!("belongs to group {:?}, not {id:?}", record.id),
            ));
        }
        Artifact::Present(record)
    }

    pub(crate) fn inspect_group(&self, id: &str, with_costs: bool) -> Artifact<Inspected> {
        let root = present!(self.open_root());
        self.inspect_in(&root, id, with_costs)
    }

    fn inspect_in(&self, root: &Dir, id: &str, with_costs: bool) -> Artifact<Inspected> {
        let group_dir = present!(open_group_dir(root, id));
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
                root,
                group_dir: &group_dir,
                index,
                member,
                process,
                with_costs,
            });
            members.push(snapshot);
            reports.push(report);
        }
        let unread = record.members.len().saturating_sub(members.len());
        let phases: Vec<MemberPhase> = members.iter().map(|member| member.phase).collect();
        // The record on disk can lag the process that waited for the last exit.
        let finished_at = record.finished_at.clone().or_else(|| {
            let exits: Option<Vec<&str>> = (unread == 0)
                .then(|| {
                    members
                        .iter()
                        .map(|member| match &member.process {
                            ProcessStatus::Exited(exit) => Some(exit.finished_at.as_str()),
                            _ => None,
                        })
                        .collect()
                })
                .flatten();
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
                phase: state::group_phase(&phases, unread),
                candidate: group_dir.read_text_head(OVERLAY_FILE, self.limits.max_overlay_bytes),
                members,
                members_omitted: unread,
                members_total: record.members.len(),
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
        let dir = open_member_dir(root, member);
        let mut problems = Vec::new();
        let (manifest, report) = match &dir {
            Artifact::Present(dir) => (
                dir.read_json::<Manifest>(output::MANIFEST_FILE, limit),
                dir.read_json::<Report>(output::REPORT_JSON_FILE, limit),
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
        let runs = unique_runs(manifest.present().map_or(&[], |m| m.runs.as_slice()));
        let tally = plan.tally(&runs, activity, manifest_finished == Some(true));
        let specs = self.trial_specs(&plan, &runs);
        let runs_dir = match &dir {
            Artifact::Present(dir) => dir.open_dir(RUNS_DIR),
            Artifact::Missing => Artifact::Missing,
            Artifact::Problem(problem) => Artifact::Problem(problem.clone()),
        };
        let trials: Vec<EvaluationTrialSnapshot> = specs
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
                .map(|dir| dir.file_ref(output::REPORT_MARKDOWN_FILE)),
            dir: path_of(&dir),
            pid: member.pid,
            phase,
            process: process.clone(),
            started_at: manifest.present().map(|m| m.started_at.clone()),
            progress,
            cost: member_cost(full_report.as_ref(), &trials, &plan.label_totals(&runs)),
            trials,
            trials_omitted: tally.total.saturating_sub(specs.len()),
            tally,
            report: report_status,
            failure,
            stderr_tail,
            problems,
        };
        (snapshot, full_report)
    }

    /// The trials to list, planned ones first in replay order, then any the
    /// manifest holds beyond the plan, up to the configured number.
    fn trial_specs<'a>(&self, plan: &'a Plan, runs: &[&'a RunEntry]) -> Vec<TrialSpec<'a>> {
        let max = self.limits.max_trials;
        let mut specs = Vec::new();
        'plan: for label in &plan.labels {
            for trial in 1..=plan.repeat {
                if specs.len() >= max {
                    break 'plan;
                }
                let entry = runs
                    .iter()
                    .copied()
                    .find(|entry| entry.label == *label && entry.index == trial);
                specs.push((label.as_str(), trial, entry));
            }
        }
        for entry in runs
            .iter()
            .copied()
            .filter(|entry| !plan.contains(&entry.label, entry.index))
        {
            if specs.len() >= max {
                break;
            }
            specs.push((entry.label.as_str(), entry.index, Some(entry)));
        }
        specs
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
            Artifact::Present(runs) => runs.open_dir(&name),
            Artifact::Missing => Artifact::Missing,
            Artifact::Problem(problem) => Artifact::Problem(problem.clone()),
        });
        let outcome = match &trial_dir {
            Some(Artifact::Present(dir)) if ended => dir
                .read_json::<Outcome>(output::OUTCOME_FILE, self.limits.max_json_bytes)
                .map(|outcome| outcome_of(&outcome)),
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
                (Some(prices), Some(Artifact::Present(dir)), true) => {
                    self.estimate_cost(dir, prices)
                }
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
                    database: dir.file_ref(output::DATABASE_FILE),
                    diff: dir.file_ref(output::DIFF_FILE),
                    conversation: dir.file_ref(output::CONVERSATION_FILE),
                    outcome: dir.file_ref(output::OUTCOME_FILE),
                    dir: dir.path().to_path_buf(),
                })
            }),
        }
    }

    /// Reprice a finished trial's own requests from its database.
    ///
    /// The database is opened through the descriptor that was checked (a
    /// regular file in the trial directory, not a link), as an immutable
    /// snapshot, so a path replaced meanwhile changes nothing and no `-wal`
    /// or `-shm` file is created. A `-wal` or `-journal` with content, a file
    /// over the byte budget and one with more rows than the row budget give no
    /// cost at all rather than a partial one.
    fn estimate_cost(&self, dir: &Dir, prices: &PriceTable) -> TrialCost {
        let unavailable = |reason| TrialCost::Unavailable(reason);
        let (file, length) = match dir.open_regular(output::DATABASE_FILE) {
            Artifact::Present(opened) => opened,
            Artifact::Missing => return unavailable(CostUnavailable::NoDatabase),
            Artifact::Problem(problem) => return unavailable(CostUnavailable::Unreadable(problem)),
        };
        if length > self.limits.max_database_bytes {
            return unavailable(CostUnavailable::OverBudget(DatabaseBudget::Bytes {
                limit: self.limits.max_database_bytes,
                actual: length,
            }));
        }
        for sidecar in [DATABASE_WAL, DATABASE_JOURNAL] {
            match dir.file_ref(sidecar).state {
                FileState::Missing | FileState::Present { bytes: 0 } => {}
                FileState::Present { .. } => return unavailable(CostUnavailable::InUse),
                FileState::Rejected(kind) => {
                    return unavailable(CostUnavailable::Unreadable(ArtifactProblem::new(
                        kind,
                        &dir.join(sidecar),
                        "cannot be checked",
                    )));
                }
            }
        }
        artifact::sync_point("opened_database", &dir.join(output::DATABASE_FILE));
        let database = dir.join(output::DATABASE_FILE);
        let pinned = pinned_path(&file);
        let rows = storage::Database::open_read_only_immutable(&pinned).and_then(|opened| {
            opened.usage_requests_between_limited(i64::MIN, i64::MAX, self.limits.max_database_rows)
        });
        // The descriptor stays open until the query is done: it pins the file.
        drop(file);
        match rows {
            Ok((_, true)) => unavailable(CostUnavailable::OverBudget(DatabaseBudget::Rows {
                limit: self.limits.max_database_rows,
            })),
            Ok((rows, false)) => {
                let requests: Vec<RequestUsage> = rows
                    .iter()
                    .map(|row| RequestUsage::from_record(&row.record))
                    .collect();
                TrialCost::Estimated(prices.cost(requests.iter().filter(|r| !r.is_overhead())))
            }
            Err(error) => unavailable(database_failure(&database, &error)),
        }
    }
}

const DATABASE_WAL: &str = "evorch.db-wal";
const DATABASE_JOURNAL: &str = "evorch.db-journal";

/// Where the kernel exposes an open descriptor as a path, so a library that
/// opens by path reaches the file the descriptor refers to.
fn pinned_path(file: &std::fs::File) -> PathBuf {
    use std::os::fd::AsRawFd;
    let prefix = if cfg!(target_os = "linux") {
        "/proc/self/fd"
    } else {
        "/dev/fd"
    };
    PathBuf::from(format!("{prefix}/{}", file.as_raw_fd()))
}

fn database_failure(path: &Path, error: &storage::StorageError) -> CostUnavailable {
    use storage::FailureClass;
    let kind = match error.class() {
        FailureClass::SchemaUnsupported => ProblemKind::UnsupportedSchema,
        FailureClass::Corrupt => ProblemKind::Malformed,
        FailureClass::Permission => ProblemKind::PermissionDenied,
        FailureClass::Busy
        | FailureClass::CannotOpen
        | FailureClass::Limit
        | FailureClass::Other => ProblemKind::Io,
    };
    CostUnavailable::Unreadable(ArtifactProblem::new(kind, path, error.to_string()))
}

/// A trial to list: its label, its repeat number and the manifest's entry for it.
type TrialSpec<'a> = (&'a str, u32, Option<&'a RunEntry>);

struct MemberInput<'a> {
    root: &'a Dir,
    group_dir: &'a Dir,
    index: usize,
    member: &'a Member,
    process: ProcessStatus,
    with_costs: bool,
}

struct TrialInput<'a> {
    label: &'a str,
    index: u32,
    entry: Option<&'a RunEntry>,
    runs_dir: &'a Artifact<Dir>,
    activity: Activity,
    manifest_finished: bool,
    report: Option<&'a Report>,
    with_costs: bool,
}

/// The manifest's entries with the last of any repeated `(label, index)`, in
/// file order.
fn unique_runs(runs: &[RunEntry]) -> Vec<&RunEntry> {
    let mut seen = HashSet::new();
    let mut unique: Vec<&RunEntry> = runs
        .iter()
        .rev()
        .filter(|entry| seen.insert((entry.label.as_str(), entry.index)))
        .collect();
    unique.reverse();
    unique
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
        let (configured, repeat) = manifest.map_or_else(
            || {
                (
                    vec![BASELINE.to_owned(), CANDIDATE.to_owned()],
                    launched_repeat,
                )
            },
            |manifest| (manifest.configurations.clone(), manifest.repeat),
        );
        let mut labels: Vec<String> = Vec::new();
        for label in configured {
            if !labels.contains(&label) {
                labels.push(label);
            }
        }
        Self { labels, repeat }
    }

    fn size(&self) -> usize {
        self.labels.len().saturating_mul(self.repeat as usize)
    }

    fn contains(&self, label: &str, index: u32) -> bool {
        self.labels.iter().any(|known| known == label) && (1..=self.repeat).contains(&index)
    }

    /// The states of all trials, planned and extra, from the manifest alone.
    fn tally(&self, runs: &[&RunEntry], activity: Activity, manifest_finished: bool) -> TrialTally {
        let mut tally = TrialTally {
            planned: self.size(),
            ..TrialTally::default()
        };
        let mut planned_started = 0_usize;
        let count = |tally: &mut TrialTally, state: TrialState, times: usize| {
            let slot = match state {
                TrialState::Queued => &mut tally.queued,
                TrialState::Running => &mut tally.running,
                TrialState::Finished => &mut tally.finished,
                TrialState::Failed => &mut tally.failed,
                TrialState::Interrupted => &mut tally.interrupted,
                TrialState::Unavailable => &mut tally.unavailable,
                TrialState::Unverified => &mut tally.unverified,
            };
            *slot = slot.saturating_add(times);
        };
        let mut extras = 0_usize;
        for entry in runs {
            if self.contains(&entry.label, entry.index) {
                planned_started += 1;
            } else {
                extras += 1;
            }
            let state = state::trial_state(Some(entry), activity, manifest_finished);
            count(&mut tally, state, 1);
        }
        let waiting = state::trial_state(None, activity, manifest_finished);
        count(
            &mut tally,
            waiting,
            self.size().saturating_sub(planned_started),
        );
        tally.total = self.size().saturating_add(extras);
        tally
    }

    /// Each configuration's trial count, extras included.
    fn label_totals(&self, runs: &[&RunEntry]) -> Vec<(String, u64)> {
        let mut totals: Vec<(String, u64)> = self
            .labels
            .iter()
            .map(|label| (label.clone(), u64::from(self.repeat)))
            .collect();
        for entry in runs.iter().filter(|e| !self.contains(&e.label, e.index)) {
            match totals.iter_mut().find(|(label, _)| *label == entry.label) {
                Some((_, count)) => *count += 1,
                None => totals.push((entry.label.clone(), 1)),
            }
        }
        totals
    }
}

fn is_group_id(id: &str) -> bool {
    !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

fn open_group_dir(root: &Dir, id: &str) -> Artifact<Dir> {
    if !is_group_id(id) {
        return Artifact::Problem(ArtifactProblem::new(
            ProblemKind::UnsafePath,
            root.path(),
            format!("{id:?} is not an evaluation group id"),
        ));
    }
    let groups = present!(root.open_dir(GROUPS_DIR));
    groups.open_dir(id)
}

/// A member's output directory: directly under the root, named as recorded,
/// and not a link.
fn open_member_dir(root: &Dir, member: &Member) -> Artifact<Dir> {
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
        Some(Ok(parent)) if parent == root.path() => root.open_dir(name),
        _ => unsafe_path(format!(
            "is not directly under the evaluation root {}",
            root.path().display()
        )),
    }
}

fn path_of(dir: &Artifact<Dir>) -> Artifact<PathBuf> {
    match dir {
        Artifact::Present(dir) => Artifact::Present(dir.path().to_path_buf()),
        Artifact::Missing => Artifact::Missing,
        Artifact::Problem(problem) => Artifact::Problem(problem.clone()),
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

/// `totals` holds each configuration's trial count over all trials, listed or
/// not, so a cost that covers only some of them says so.
fn member_cost(
    report: Option<&Report>,
    trials: &[EvaluationTrialSnapshot],
    totals: &[(String, u64)],
) -> MemberCost {
    if let Some(report) = report {
        return MemberCost {
            source: CostSource::Report,
            configurations: report
                .costs
                .iter()
                .map(|cost| ConfigurationCostSnapshot {
                    label: cost.label.clone(),
                    counted: u64::from(cost.runs),
                    planned: u64::from(cost.runs),
                    cost: cost.cost.clone(),
                })
                .collect(),
        };
    }
    let mut configurations: Vec<ConfigurationCostSnapshot> = Vec::new();
    for trial in trials {
        let TrialCost::Estimated(cost) = &trial.cost else {
            continue;
        };
        let position = configurations
            .iter()
            .position(|known| known.label == trial.label);
        let slot = match position {
            Some(position) => &mut configurations[position],
            None => {
                let planned = totals
                    .iter()
                    .find(|(label, _)| *label == trial.label)
                    .map_or(1, |(_, count)| *count);
                configurations.push(ConfigurationCostSnapshot {
                    label: trial.label.clone(),
                    counted: 0,
                    planned,
                    cost: Cost::default(),
                });
                configurations.last_mut().expect("just pushed")
            }
        };
        slot.counted += 1;
        slot.cost.add(cost);
    }
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
fn stderr_tail(group_dir: &Dir, recorded: &Path, problems: &mut Vec<ArtifactProblem>) -> String {
    let Some(name) = recorded.file_name().and_then(OsStr::to_str) else {
        return String::new();
    };
    match group_dir.read_text_tail(name, STDERR_BYTES) {
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
