#![allow(dead_code)]

//! Fixtures shared by the Evaluations panel tests: inspection snapshots built
//! by hand and a reader whose reads the test controls.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, Mutex, PoisonError};

use eval::cost::Cost;
use eval::driver::Status;
use eval::inspection::{
    Artifact, BoundedText, ConfigurationCostSnapshot, CostSource, CostUnavailable,
    EvaluationGroupSnapshot, EvaluationMemberSnapshot, EvaluationTrialSnapshot, FileRef, FileState,
    GroupListEntry, GroupListing, GroupPhase, MemberCost, MemberPhase, ProcessStatus, Purpose,
    ReportStatus, ReportSummary, TrialArtifacts, TrialCost, TrialOutcome, TrialProgress,
    TrialState, TrialTally,
};
use gui::model::evaluations::{EvaluationReader, ReplayLoad, ReplayTarget};

pub fn cost(usd: f64) -> Cost {
    Cost {
        usd,
        api_equivalent_usd: 0.0,
        unpriced: Vec::new(),
        unreported: Vec::new(),
    }
}

fn file(name: &str, present: bool) -> FileRef {
    FileRef {
        path: PathBuf::from(name),
        state: if present {
            FileState::Present { bytes: 10 }
        } else {
            FileState::Missing
        },
    }
}

pub struct TrialSpec {
    pub label: &'static str,
    pub index: u32,
    pub state: TrialState,
    pub cost: TrialCost,
    /// Where the trial's database is, when it has one.
    pub database: Option<PathBuf>,
}

pub fn trial(spec: TrialSpec) -> EvaluationTrialSnapshot {
    let ended = matches!(spec.state, TrialState::Finished | TrialState::Failed);
    let name = format!("{}-{}", spec.label, spec.index);
    let artifacts = ended.then(|| {
        let mut database = file(&format!("{name}/evorch.db"), spec.database.is_some());
        if let Some(path) = &spec.database {
            database.path.clone_from(path);
        }
        Artifact::Present(TrialArtifacts {
            dir: PathBuf::from(&name),
            database,
            diff: file("diff.patch", false),
            conversation: file("conversation.json", false),
            outcome: file("outcome.json", true),
        })
    });
    EvaluationTrialSnapshot {
        label: spec.label.to_owned(),
        index: spec.index,
        state: spec.state,
        started_at: Some("2025-01-01T00:00:00Z".into()),
        finished_at: ended.then(|| "2025-01-01T00:01:00Z".into()),
        status: ended.then_some(Status::Completed),
        outcome: if ended {
            Artifact::Present(TrialOutcome {
                status: Status::Completed,
                reason: String::new(),
                user_turns: 2,
                tokens: 1234,
                tool_calls: 3,
                elapsed_ms: 60_000,
                denied_requests: 0,
                pushed_refs: 0,
                verification_passed: Some(true),
            })
        } else {
            Artifact::Missing
        },
        cost: spec.cost,
        artifacts,
    }
}

pub struct MemberSpec {
    pub index: usize,
    pub thread: &'static str,
    pub purpose: Purpose,
    pub phase: MemberPhase,
    pub trials: Vec<EvaluationTrialSnapshot>,
    pub planned: usize,
    pub report: ReportStatus,
    pub markdown: bool,
    pub cost: MemberCost,
}

pub fn member(spec: MemberSpec) -> EvaluationMemberSnapshot {
    let finished = spec
        .trials
        .iter()
        .filter(|trial| matches!(trial.state, TrialState::Finished))
        .count();
    let tally = TrialTally {
        planned: spec.planned,
        total: spec.trials.len(),
        finished,
        ..TrialTally::default()
    };
    EvaluationMemberSnapshot {
        index: spec.index,
        thread_id: spec.thread.to_owned(),
        purpose: spec.purpose,
        repeat: 1,
        dir_name: format!("member-{}", spec.index),
        dir: Artifact::Missing,
        pid: None,
        phase: spec.phase,
        process: ProcessStatus::Unknown,
        started_at: Some("2025-01-01T00:00:00Z".into()),
        progress: Some(TrialProgress {
            planned: spec.planned,
            started: spec.trials.len(),
            finished,
            manifest_finished: false,
        }),
        trials: spec.trials,
        trials_omitted: 0,
        tally,
        report: spec.report,
        report_markdown: spec.markdown.then(|| file("report.md", true)),
        cost: spec.cost,
        failure: None,
        stderr_tail: None,
        problems: Vec::new(),
    }
}

pub fn no_cost() -> MemberCost {
    MemberCost {
        source: CostSource::None,
        configurations: Vec::new(),
    }
}

pub fn estimated(usd: f64, counted: u64, planned: u64) -> MemberCost {
    MemberCost {
        source: CostSource::Estimate,
        configurations: vec![ConfigurationCostSnapshot {
            label: "baseline".into(),
            counted,
            planned,
            cost: cost(usd),
        }],
    }
}

pub fn available_report() -> ReportStatus {
    ReportStatus::Available(Box::new(ReportSummary {
        generated_at: "2025-01-01T00:02:00Z".into(),
        pairs_judged: 2,
        tally: Vec::new(),
        costs: Vec::new(),
        warnings: 0,
    }))
}

pub fn group(
    id: &str,
    phase: GroupPhase,
    members: Vec<EvaluationMemberSnapshot>,
) -> EvaluationGroupSnapshot {
    EvaluationGroupSnapshot {
        id: id.to_owned(),
        run_id: "run-1".into(),
        started_at: "2025-01-01T00:00:00Z".into(),
        finished_at: None,
        notice: None,
        phase,
        candidate: Artifact::Missing,
        members_total: members.len(),
        members_omitted: 0,
        members,
    }
}

pub fn listing(entries: Vec<GroupListEntry>) -> GroupListing {
    GroupListing {
        entries,
        omitted: 0,
        scan_truncated: false,
        problem: None,
    }
}

pub fn entry(snapshot: EvaluationGroupSnapshot) -> GroupListEntry {
    GroupListEntry {
        id: snapshot.id.clone(),
        group: Artifact::Present(snapshot),
    }
}

pub fn unavailable_cost() -> TrialCost {
    TrialCost::Unavailable(CostUnavailable::NotFinished)
}

/// A reader that answers from values the test sets, counts what is asked and
/// can hold its first listing until the test lets it go.
pub struct FakeReader {
    pub listing: Mutex<GroupListing>,
    pub groups: Mutex<HashMap<String, Artifact<EvaluationGroupSnapshot>>>,
    pub markdown: Mutex<HashMap<(String, usize), Artifact<BoundedText>>>,
    pub replay: Mutex<Option<ReplayLoad>>,
    pub list_calls: AtomicUsize,
    pub group_calls: AtomicUsize,
    pub replay_calls: AtomicUsize,
    /// `(entered, release)`: the first listing waits on both.
    pub gate: Mutex<Option<(Arc<Barrier>, Arc<Barrier>)>>,
    gated: AtomicBool,
}

impl FakeReader {
    pub fn new(listing: GroupListing) -> Arc<Self> {
        Arc::new(Self {
            listing: Mutex::new(listing),
            groups: Mutex::new(HashMap::new()),
            markdown: Mutex::new(HashMap::new()),
            replay: Mutex::new(None),
            list_calls: AtomicUsize::new(0),
            group_calls: AtomicUsize::new(0),
            replay_calls: AtomicUsize::new(0),
            gate: Mutex::new(None),
            gated: AtomicBool::new(true),
        })
    }

    pub fn with_group(self: &Arc<Self>, group: EvaluationGroupSnapshot) -> Arc<Self> {
        self.groups
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(group.id.clone(), Artifact::Present(group));
        Arc::clone(self)
    }

    pub fn with_markdown(self: &Arc<Self>, group: &str, member: usize, text: &str) -> Arc<Self> {
        self.markdown
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(
                (group.to_owned(), member),
                Artifact::Present(BoundedText {
                    text: text.to_owned(),
                    total_bytes: text.len() as u64,
                    truncated: false,
                }),
            );
        Arc::clone(self)
    }
}

impl EvaluationReader for FakeReader {
    fn list(&self, _limit: usize) -> GroupListing {
        self.list_calls.fetch_add(1, Ordering::SeqCst);
        if self.gated.swap(false, Ordering::SeqCst) {
            let gate = self
                .gate
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone();
            if let Some((entered, release)) = gate {
                entered.wait();
                release.wait();
            }
        }
        self.listing
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn group(&self, id: &str) -> Artifact<EvaluationGroupSnapshot> {
        self.group_calls.fetch_add(1, Ordering::SeqCst);
        self.groups
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(id)
            .cloned()
            .unwrap_or(Artifact::Missing)
    }

    fn report_markdown(&self, group: &str, member: usize, _max: u64) -> Artifact<BoundedText> {
        self.markdown
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&(group.to_owned(), member))
            .cloned()
            .unwrap_or(Artifact::Missing)
    }

    fn replay(&self, _target: &ReplayTarget) -> ReplayLoad {
        self.replay_calls.fetch_add(1, Ordering::SeqCst);
        self.replay
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
            .unwrap_or(ReplayLoad::Fallback {
                reason: "no replay in this fixture".into(),
                conversation: Artifact::Missing,
            })
    }
}
