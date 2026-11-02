use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use config::{EvalBudgetConfig, RoleBindingConfig};

use super::*;
use crate::cost::Cost;
use crate::driver::Status;
use crate::metrics::{RunMetrics, RunRecord};
use crate::output::{BindingSummary, Outcome, StartSummary};
use crate::report::{ConfigurationCost, Overhead, PriceSource, RunSummary};
use crate::test_support::{usage, write_run_db};

const GROUP: &str = "20261020-000000-abc123";
const CONFIG: &str = r#"version = 3

[providers.main]
provider_type = "openai"
default_model = "gpt-test"
models = [{ id = "gpt-test", enabled = true, input_price = 2.0, output_price = 10.0 }]
"#;

struct Fixture {
    _temp: tempfile::TempDir,
    /// The canonical `eval` directory.
    root: PathBuf,
    outside: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().canonicalize().unwrap();
        let root = base.join("eval");
        let outside = base.join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        Self {
            _temp: temp,
            root,
            outside,
        }
    }

    fn inspector(&self) -> EvaluationInspector {
        EvaluationInspector::new(&self.root)
    }

    fn group_dir(&self, id: &str) -> PathBuf {
        self.root.join(GROUPS_DIR).join(id)
    }

    fn member_dir(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }

    /// Write a group of one member per `(thread, directory name, exit)` and
    /// create no member directory.
    fn group(&self, id: &str, members: &[(&str, &str, Option<ProcessExit>)]) -> GroupRecord {
        let dir = self.group_dir(id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(OVERLAY_FILE), "[agents.worker]\nmodel = \"x\"\n").unwrap();
        let record = GroupRecord {
            schema: GROUP_SCHEMA,
            id: id.into(),
            run_id: "run-7".into(),
            started_at: "2026-10-20T00:00:00.000Z".into(),
            candidate: dir.join(OVERLAY_FILE),
            members: members
                .iter()
                .enumerate()
                .map(|(index, (thread, name, exit))| Member {
                    thread_id: (*thread).into(),
                    purpose: if index == 0 {
                        Purpose::Target
                    } else {
                        Purpose::Regression
                    },
                    repeat: 1,
                    dir: self.member_dir(name),
                    pid: Some(std::process::id()),
                    stdout: dir.join(format!("{index}.stdout.log")),
                    stderr: dir.join(format!("{index}.stderr.log")),
                    exit: exit.clone(),
                })
                .collect(),
            finished_at: None,
            notice: None,
        };
        output::write_json(&dir.join(GROUP_FILE), &record).unwrap();
        record
    }

    /// A group launched by this process, which the monitor knows.
    fn launched(&self, record: GroupRecord) -> (LaunchMonitor, Arc<Mutex<GroupRecord>>) {
        let monitor = LaunchMonitor::default();
        let record = Arc::new(Mutex::new(record));
        monitor.insert(Arc::clone(&record));
        (monitor, record)
    }
}

fn prices() -> Arc<PriceTable> {
    let config = tempfile::tempdir().unwrap();
    std::fs::write(config.path().join("config.toml"), CONFIG).unwrap();
    let providers = crate::runner::load_config(Some(config.path()), None, None)
        .unwrap()
        .providers;
    Arc::new(PriceTable::new(providers, None))
}

/// Removes the sync hook when the test ends, however it ends.
struct HookGuard;

impl Drop for HookGuard {
    fn drop(&mut self) {
        artifact::set_sync_hook(None);
    }
}

/// Run `action` once, the first time something named `name` whose path ends in
/// `suffix` has just been opened.
fn on_open(
    name: &'static str,
    suffix: &'static str,
    action: impl Fn(&Path) + 'static,
) -> HookGuard {
    let done = std::cell::Cell::new(false);
    artifact::set_sync_hook(Some(Box::new(move |point, path| {
        if point == name && path.ends_with(suffix) && !done.replace(true) {
            action(path);
        }
    })));
    HookGuard
}

/// Put a symlink to `target` where `path` is, keeping what was there.
fn replace_with_link(path: &Path, target: &Path) {
    let mut kept = path.as_os_str().to_owned();
    kept.push(".original");
    std::fs::rename(path, kept).unwrap();
    std::os::unix::fs::symlink(target, path).unwrap();
}

/// A member whose only trial finished, with a database holding `rows`, and
/// the trial's directory.
fn finished_trial_with_database(
    fixture: &Fixture,
    rows: Vec<storage::usage::UsageRequestRecord>,
) -> PathBuf {
    fixture.group(
        GROUP,
        &[("thread-1", "member-1", Some(exit(None, Some(15), None)))],
    );
    let member = fixture.member_dir("member-1");
    std::fs::create_dir_all(&member).unwrap();
    output::write_json(
        &member.join(output::MANIFEST_FILE),
        &manifest(
            &["baseline"],
            1,
            vec![run("baseline", 1, Some(Status::Completed))],
            true,
        ),
    )
    .unwrap();
    let trial = write_trial(&member, "baseline", 1, Status::Completed);
    write_run_db(&trial.join(output::DATABASE_FILE), &[], rows);
    trial
}

fn first_cost(inspector: &EvaluationInspector) -> TrialCost {
    member_of(inspector.group(GROUP)).trials.remove(0).cost
}

fn exit(code: Option<i32>, signal: Option<i32>, error: Option<&str>) -> ProcessExit {
    ProcessExit::now(code, signal, error.map(str::to_owned))
}

fn run(label: &str, index: u32, status: Option<Status>) -> RunEntry {
    RunEntry {
        label: label.into(),
        index,
        dir: format!("runs/{label}-{index}"),
        nonce: "nonce".into(),
        started_at: "2026-10-20T00:00:01.000Z".into(),
        finished_at: status.map(|_| "2026-10-20T00:01:00.000Z".into()),
        status,
    }
}

fn manifest(configurations: &[&str], repeat: u32, runs: Vec<RunEntry>, finished: bool) -> Manifest {
    Manifest {
        thread_id: "thread-1".into(),
        source_db: PathBuf::from("/db"),
        start: StartSummary {
            project_root: PathBuf::from("/p"),
            repository: PathBuf::from("/p"),
            repo_prefix: String::new(),
            head_commit: "abc".into(),
            head_ref: None,
            diff_bytes: 0,
            untracked_entries: 0,
            captured_at_ns: 1,
        },
        original_turns: Vec::new(),
        problem: None,
        verify: None,
        candidate_overlay: None,
        swapped_roles: Vec::new(),
        configurations: configurations.iter().map(|c| (*c).to_owned()).collect(),
        repeat,
        simulator: BindingSummary {
            source: "agents.worker".into(),
            binding: RoleBindingConfig::default(),
        },
        budget: EvalBudgetConfig::default(),
        started_at: "2026-10-20T00:00:00.000Z".into(),
        finished_at: finished.then(|| "2026-10-20T00:20:00.000Z".into()),
        runs,
    }
}

fn outcome(status: Status) -> Outcome {
    Outcome {
        status,
        reason: "done".into(),
        user_turns: 2,
        tokens: 10,
        tool_calls: 3,
        elapsed_ms: 1_000,
        denied_requests: Vec::new(),
        pushed_refs: Vec::new(),
        verification: None,
    }
}

fn priced(usd: f64) -> Cost {
    Cost {
        usd,
        ..Cost::default()
    }
}

/// A report whose runs are the given `(label, index, usd)`.
fn report(runs: &[(&str, u32, f64)]) -> Report {
    let outcome = outcome(Status::Completed);
    let run_summaries = runs
        .iter()
        .map(|(label, index, usd)| RunSummary {
            label: (*label).into(),
            index: *index,
            dir: format!("runs/{label}-{index}"),
            status: Status::Completed,
            reason: "done".into(),
            metrics: RunMetrics::of(&RunRecord::default(), &outcome),
            cost: priced(*usd),
            verification: None,
            external_dependencies: Vec::new(),
            pushed_refs: 0,
        })
        .collect();
    let mut labels: Vec<&str> = runs.iter().map(|(label, ..)| *label).collect();
    labels.dedup();
    Report {
        thread_id: "thread-1".into(),
        problem: None,
        verify: None,
        configurations: labels.iter().map(|l| (*l).to_owned()).collect(),
        candidate_overlay: None,
        swapped_roles: Vec::new(),
        repeat: 1,
        budget: EvalBudgetConfig::default(),
        simulator: BindingSummary {
            source: "s".into(),
            binding: RoleBindingConfig::default(),
        },
        judge: BindingSummary {
            source: "j".into(),
            binding: RoleBindingConfig::default(),
        },
        prices: PriceSource {
            catalog_fetched_at: None,
            note: "configured prices".into(),
        },
        runs: run_summaries,
        costs: labels
            .iter()
            .map(|label| ConfigurationCost {
                label: (*label).into(),
                runs: runs.iter().filter(|(l, ..)| l == label).count() as u32,
                cost: priced(
                    runs.iter()
                        .filter(|(l, ..)| l == label)
                        .map(|(.., usd)| usd)
                        .sum(),
                ),
            })
            .collect(),
        overhead: Overhead::default(),
        pairs: Vec::new(),
        tally: Vec::new(),
        warnings: Vec::new(),
        generated_at: "2026-10-20T00:30:00.000Z".into(),
    }
}

fn write_trial(member: &Path, label: &str, index: u32, status: Status) -> PathBuf {
    let dir = member.join("runs").join(format!("{label}-{index}"));
    std::fs::create_dir_all(&dir).unwrap();
    output::write_json(&dir.join(output::OUTCOME_FILE), &outcome(status)).unwrap();
    dir
}

fn states(member: &EvaluationMemberSnapshot) -> Vec<(String, TrialState)> {
    member
        .trials
        .iter()
        .map(|trial| (trial.name(), trial.state))
        .collect()
}

fn member_of(snapshot: Artifact<EvaluationGroupSnapshot>) -> EvaluationMemberSnapshot {
    snapshot.into_present().expect("a group").members.remove(0)
}

#[test]
fn a_launched_evaluation_goes_from_queued_through_running_and_reporting_to_finished() {
    // Given: a group this process launched whose member has written nothing yet.
    let fixture = Fixture::new();
    let record = fixture.group(GROUP, &[("thread-1", "member-1", None)]);
    let member_dir = fixture.member_dir("member-1");
    let (monitor, live) = fixture.launched(record);
    let inspector = fixture.inspector().with_monitor(monitor);

    // Then: it is starting and every planned trial waits.
    let member = member_of(inspector.group(GROUP));
    assert_eq!(member.phase, MemberPhase::Starting);
    assert_eq!(member.process, ProcessStatus::Running);
    assert_eq!(
        states(&member),
        [
            ("baseline-1".into(), TrialState::Queued),
            ("candidate-1".into(), TrialState::Queued)
        ]
    );
    assert!(member.dir.is_missing());

    // When: the first trial starts.
    std::fs::create_dir_all(&member_dir).unwrap();
    let started = vec![run("baseline", 1, None)];
    output::write_json(
        &member_dir.join(output::MANIFEST_FILE),
        &manifest(&["baseline", "candidate"], 1, started, false),
    )
    .unwrap();

    // Then: it runs and the other still waits.
    let member = member_of(inspector.group(GROUP));
    assert_eq!(member.phase, MemberPhase::RunningTrials);
    assert_eq!(
        states(&member),
        [
            ("baseline-1".into(), TrialState::Running),
            ("candidate-1".into(), TrialState::Queued)
        ]
    );
    let progress = member.progress.unwrap();
    assert_eq!(
        (progress.planned, progress.started, progress.finished),
        (2, 1, 0)
    );

    // When: the first trial ends and the second starts.
    write_trial(&member_dir, "baseline", 1, Status::Completed);
    let runs = vec![
        run("baseline", 1, Some(Status::Completed)),
        run("candidate", 1, None),
    ];
    output::write_json(
        &member_dir.join(output::MANIFEST_FILE),
        &manifest(&["baseline", "candidate"], 1, runs, false),
    )
    .unwrap();

    // Then: the first has its outcome and the second runs.
    let member = member_of(inspector.group(GROUP));
    assert_eq!(member.trials[0].state, TrialState::Finished);
    assert_eq!(member.trials[0].status, Some(Status::Completed));
    let found = member.trials[0].outcome.present().expect("an outcome");
    assert_eq!((found.status, found.tool_calls), (Status::Completed, 3));
    assert_eq!(member.trials[1].state, TrialState::Running);
    assert_eq!(member.finished_trials(), 1);

    // When: every trial ended and the report is still being made.
    write_trial(&member_dir, "candidate", 1, Status::Incomplete);
    let runs = vec![
        run("baseline", 1, Some(Status::Completed)),
        run("candidate", 1, Some(Status::Incomplete)),
    ];
    output::write_json(
        &member_dir.join(output::MANIFEST_FILE),
        &manifest(&["baseline", "candidate"], 1, runs, true),
    )
    .unwrap();

    // Then: trials are done while the process and report are not.
    let group = inspector.group(GROUP).into_present().unwrap();
    let member = &group.members[0];
    assert_eq!(member.phase, MemberPhase::Reporting);
    assert_eq!(member.report, ReportStatus::Pending);
    assert_eq!(group.phase, GroupPhase::Running);
    assert!(
        member
            .trials
            .iter()
            .all(|trial| trial.state == TrialState::Finished)
    );
    assert!(!member.trials[0].cost.has_cost());

    // When: the report appears before the process is seen to end.
    output::write_json(
        &member_dir.join(output::REPORT_JSON_FILE),
        &report(&[("baseline", 1, 1.0), ("candidate", 1, 0.5)]),
    )
    .unwrap();

    // Then: the report is available and the costs come from it.
    let member = member_of(inspector.group(GROUP));
    assert!(member.report.is_available());
    assert_eq!(member.phase, MemberPhase::Reporting);
    assert_eq!(member.trials[0].cost, TrialCost::Reported(priced(1.0)));
    assert_eq!(member.cost.source, CostSource::Report);
    assert_eq!(member.cost.configurations[1].cost.usd, 0.5);

    // When: the process ends, known to this process before the record is saved.
    live.lock().unwrap().members[0].exit = Some(exit(Some(0), None, None));

    // Then: the member finished although the file still has no exit.
    let group = inspector.group(GROUP).into_present().unwrap();
    assert_eq!(group.members[0].phase, MemberPhase::Finished);
    assert_eq!(group.phase, GroupPhase::Finished);
    assert!(group.finished_at.is_some());
}

#[test]
fn a_process_that_could_not_start_leaves_no_trial_running() {
    // Given: a launch failure recorded in the group.
    let fixture = Fixture::new();
    let failure = exit(None, None, Some("cannot start /bin/evorch: not found"));
    fixture.group(GROUP, &[("thread-1", "member-1", Some(failure))]);

    // When: the group is inspected without any monitor.
    let group = fixture.inspector().group(GROUP).into_present().unwrap();
    let member = &group.members[0];

    // Then: it failed with the reason, and nothing is queued or running.
    assert_eq!(member.phase, MemberPhase::Failed);
    assert_eq!(
        member.failure.as_deref(),
        Some("cannot start /bin/evorch: not found")
    );
    assert_eq!(member.stderr_tail.as_deref(), Some(""));
    assert!(
        member
            .trials
            .iter()
            .all(|trial| trial.state == TrialState::Unavailable)
    );
    assert_eq!(group.phase, GroupPhase::Incomplete);
    assert_eq!(member.phase.legacy_state(), "failed");
}

#[test]
fn a_process_killed_by_a_signal_interrupts_its_started_trial_and_cancels_the_rest() {
    // Given: a member killed while its first of three trials ran.
    let fixture = Fixture::new();
    fixture.group(
        GROUP,
        &[("thread-1", "member-1", Some(exit(None, Some(9), None)))],
    );
    let stderr = fixture.group_dir(GROUP).join("0.stderr.log");
    std::fs::write(&stderr, format!("{}\nout of memory\n", "x".repeat(40_000))).unwrap();
    let member_dir = fixture.member_dir("member-1");
    std::fs::create_dir_all(&member_dir).unwrap();
    let runs = vec![
        run("baseline", 1, Some(Status::Completed)),
        run("baseline", 2, None),
    ];
    output::write_json(
        &member_dir.join(output::MANIFEST_FILE),
        &manifest(&["baseline"], 3, runs, false),
    )
    .unwrap();
    write_trial(&member_dir, "baseline", 1, Status::Completed);

    // When / Then: the started trial is interrupted, the unstarted one unavailable.
    let member = member_of(fixture.inspector().group(GROUP));
    assert_eq!(member.phase, MemberPhase::Failed);
    assert_eq!(member.failure.as_deref(), Some("killed by signal 9"));
    assert_eq!(
        states(&member),
        [
            ("baseline-1".into(), TrialState::Finished),
            ("baseline-2".into(), TrialState::Interrupted),
            ("baseline-3".into(), TrialState::Unavailable)
        ]
    );
    let tail = member.stderr_tail.unwrap();
    assert!(tail.ends_with("out of memory"), "{tail}");
    assert!(tail.chars().count() <= 1_200);
}

#[test]
fn a_clean_exit_without_a_report_is_finished_without_one() {
    // Given: a member that exited 0 after all trials, leaving no report.
    let fixture = Fixture::new();
    fixture.group(
        GROUP,
        &[("thread-1", "member-1", Some(exit(Some(0), None, None)))],
    );
    let member_dir = fixture.member_dir("member-1");
    std::fs::create_dir_all(&member_dir).unwrap();
    let runs = vec![run("baseline", 1, Some(Status::Completed))];
    output::write_json(
        &member_dir.join(output::MANIFEST_FILE),
        &manifest(&["baseline"], 1, runs, true),
    )
    .unwrap();
    write_trial(&member_dir, "baseline", 1, Status::Completed);

    // When / Then: the trial finished, the report is missing, not pending.
    let group = fixture.inspector().group(GROUP).into_present().unwrap();
    let member = &group.members[0];
    assert_eq!(member.phase, MemberPhase::FinishedWithoutReport);
    assert_eq!(member.report, ReportStatus::Missing);
    assert_eq!(member.trials[0].state, TrialState::Finished);
    assert_eq!(group.phase, GroupPhase::Incomplete);
    assert_eq!(member.phase.legacy_state(), "finished");
}

#[test]
fn a_group_nobody_can_vouch_for_is_unverified_not_running() {
    // Given: an unfinished group record from another process, whose recorded
    // pid is that of a live process (this one).
    let fixture = Fixture::new();
    fixture.group(GROUP, &[("thread-1", "member-1", None)]);
    let member_dir = fixture.member_dir("member-1");
    std::fs::create_dir_all(&member_dir).unwrap();
    let runs = vec![run("baseline", 1, None)];
    output::write_json(
        &member_dir.join(output::MANIFEST_FILE),
        &manifest(&["baseline", "candidate"], 1, runs, false),
    )
    .unwrap();

    // When: it is inspected, with and without a monitor that never launched it.
    let plain = fixture.inspector().group(GROUP).into_present().unwrap();
    let foreign = fixture
        .inspector()
        .with_monitor(LaunchMonitor::default())
        .group(GROUP)
        .into_present()
        .unwrap();

    // Then: neither claims it runs, and no trial is running or queued.
    for group in [&plain, &foreign] {
        let member = &group.members[0];
        assert_eq!(member.pid, Some(std::process::id()));
        assert_eq!(member.process, ProcessStatus::Unknown);
        assert_eq!(member.phase, MemberPhase::Unverified);
        assert_eq!(group.phase, GroupPhase::Unverified);
        assert!(
            member
                .trials
                .iter()
                .all(|trial| trial.state == TrialState::Unverified)
        );
        assert_eq!(member.phase.legacy_state(), "interrupted");
    }
}

#[test]
fn a_finished_manifest_and_report_show_completion_even_when_the_exit_was_never_recorded() {
    // Given: an unmonitored member with finished trials and a complete report.
    let fixture = Fixture::new();
    fixture.group(GROUP, &[("thread-1", "member-1", None)]);
    let member_dir = fixture.member_dir("member-1");
    std::fs::create_dir_all(&member_dir).unwrap();
    let runs = vec![run("baseline", 1, Some(Status::Completed))];
    output::write_json(
        &member_dir.join(output::MANIFEST_FILE),
        &manifest(&["baseline"], 1, runs, true),
    )
    .unwrap();
    write_trial(&member_dir, "baseline", 1, Status::Completed);

    // Then: without the report the member stays unverified though its trial is done.
    let member = member_of(fixture.inspector().group(GROUP));
    assert_eq!(member.phase, MemberPhase::Unverified);
    assert_eq!(member.trials[0].state, TrialState::Finished);
    assert_eq!(member.report, ReportStatus::Missing);

    // When: the report appears.
    output::write_json(
        &member_dir.join(output::REPORT_JSON_FILE),
        &report(&[("baseline", 1, 1.0)]),
    )
    .unwrap();

    // Then: the member is finished.
    let member = member_of(fixture.inspector().group(GROUP));
    assert_eq!(member.phase, MemberPhase::Finished);
}

#[test]
fn trials_follow_the_replay_order_for_every_repeat_and_configuration() {
    // Given: a launched member whose manifest does not exist, with three repeats.
    let fixture = Fixture::new();
    let mut record = fixture.group(GROUP, &[("thread-1", "member-1", None)]);
    record.members[0].repeat = 3;
    output::write_json(&fixture.group_dir(GROUP).join(GROUP_FILE), &record).unwrap();
    let (monitor, _live) = fixture.launched(record);
    let inspector = fixture.inspector().with_monitor(monitor);

    // Then: baseline and candidate are both planned, baseline first.
    let member = member_of(inspector.group(GROUP));
    let names: Vec<String> = member.trials.iter().map(|trial| trial.name()).collect();
    assert_eq!(
        names,
        [
            "baseline-1",
            "baseline-2",
            "baseline-3",
            "candidate-1",
            "candidate-2",
            "candidate-3"
        ]
    );

    // When: a baseline-only manifest with two repeats appears.
    let member_dir = fixture.member_dir("member-1");
    std::fs::create_dir_all(&member_dir).unwrap();
    output::write_json(
        &member_dir.join(output::MANIFEST_FILE),
        &manifest(&["baseline"], 2, Vec::new(), false),
    )
    .unwrap();

    // Then: only the manifest's configuration is planned.
    let member = member_of(inspector.group(GROUP));
    let names: Vec<String> = member.trials.iter().map(|trial| trial.name()).collect();
    assert_eq!(names, ["baseline-1", "baseline-2"]);
    assert_eq!(member.planned_trials(), 2);
}

#[test]
fn trial_costs_are_estimated_from_finished_databases_and_unknown_costs_are_not_zero() {
    // Given: two baseline repeats, the first finished with one priced and one
    // unpriced request, the second finished with no database; and a candidate
    // still running.
    let fixture = Fixture::new();
    fixture.group(
        GROUP,
        &[("thread-1", "member-1", Some(exit(None, Some(15), None)))],
    );
    let member_dir = fixture.member_dir("member-1");
    std::fs::create_dir_all(&member_dir).unwrap();
    let runs = vec![
        run("baseline", 1, Some(Status::Completed)),
        run("baseline", 2, Some(Status::Completed)),
        run("candidate", 1, None),
    ];
    output::write_json(
        &member_dir.join(output::MANIFEST_FILE),
        &manifest(&["baseline", "candidate"], 2, runs, false),
    )
    .unwrap();
    let first = write_trial(&member_dir, "baseline", 1, Status::Completed);
    write_run_db(
        &first.join(output::DATABASE_FILE),
        &[],
        vec![
            usage("r1", "main", "gpt-test", "agent", 1_000_000),
            usage("r2", "main", "mystery-model", "agent", 1_000_000),
        ],
    );
    write_trial(&member_dir, "baseline", 2, Status::Completed);
    let prices = prices();

    // When: inspected without and with prices.
    let unpriced = member_of(fixture.inspector().group(GROUP));
    let member = member_of(fixture.inspector().with_prices(prices).group(GROUP));

    // Then: without prices no trial has a cost, and the member has none (not $0).
    assert!(unpriced.trials.iter().all(|trial| !trial.cost.has_cost()));
    assert_eq!(unpriced.cost.source, CostSource::None);
    assert!(unpriced.cost.configurations.is_empty());

    // Then: with prices the first trial is estimated and flagged incomplete.
    let TrialCost::Estimated(first) = &member.trials[0].cost else {
        panic!("{:?}", member.trials[0].cost);
    };
    assert_eq!(first.usd, 2.0);
    assert!(!first.is_complete());
    assert_eq!(first.unpriced[0].model, "mystery-model");

    // Then: a finished trial without a database and an unfinished one have no cost.
    assert_eq!(
        member.trials[1].cost,
        TrialCost::Unavailable(CostUnavailable::NoDatabase)
    );
    assert_eq!(
        member.trials[2].cost,
        TrialCost::Unavailable(CostUnavailable::NotFinished)
    );

    // Then: the member's partial cost counts one of its two baseline trials.
    assert_eq!(member.cost.source, CostSource::Estimate);
    let [baseline] = member.cost.configurations.as_slice() else {
        panic!("{:?}", member.cost);
    };
    assert_eq!((baseline.counted, baseline.planned), (1, 2));
    assert!(baseline.is_partial());
}

#[test]
fn a_listing_is_newest_first_bounded_and_keeps_unreadable_groups_visible() {
    // Given: three groups, a stray file, an oddly named entry and a group of a newer schema.
    let fixture = Fixture::new();
    for id in ["20260101-000000-aaaaaa", "20260102-000000-bbbbbb"] {
        fixture.group(id, &[("thread-1", &format!("m-{id}"), None)]);
    }
    let newer = fixture.group("20260103-000000-cccccc", &[("thread-1", "m-new", None)]);
    let mut value = serde_json::to_value(&newer).unwrap();
    value["schema"] = 99.into();
    std::fs::write(
        fixture.group_dir("20260103-000000-cccccc").join(GROUP_FILE),
        value.to_string(),
    )
    .unwrap();
    std::fs::write(fixture.root.join(GROUPS_DIR).join("not a group"), "").unwrap();
    std::fs::create_dir_all(fixture.group_dir("20260104-000000-dddddd")).unwrap();

    // When: listed with room, and with a limit of two.
    let all = fixture.inspector().list_groups(10);
    let two = fixture.inspector().list_groups(2);

    // Then: newest first, with the problems kept in place.
    let ids: Vec<&str> = all.entries.iter().map(|entry| entry.id.as_str()).collect();
    assert_eq!(
        ids,
        [
            "20260104-000000-dddddd",
            "20260103-000000-cccccc",
            "20260102-000000-bbbbbb",
            "20260101-000000-aaaaaa"
        ]
    );
    assert!(all.entries[0].group.is_missing());
    assert_eq!(
        all.entries[1].group.problem().unwrap().kind,
        ProblemKind::UnsupportedSchema
    );
    assert!(all.entries[2].group.present().is_some());
    assert_eq!(two.entries.len(), 2);
    assert_eq!(two.omitted, 2);

    // Then: a scan bound stops the directory walk and says so.
    let limited = fixture
        .inspector()
        .with_limits(InspectionLimits {
            max_scan_entries: 2,
            ..InspectionLimits::default()
        })
        .list_groups(10);
    assert!(limited.scan_truncated);
    assert!(limited.entries.len() <= 2);

    // Then: no evaluation root at all lists nothing.
    let none = EvaluationInspector::new(fixture.root.join("nowhere")).list_groups(10);
    assert!(none.entries.is_empty() && none.problem.is_none());
}

#[test]
fn group_ids_and_paths_cannot_leave_the_evaluation_root() {
    // Given: a group directory outside the root, reached through a link.
    let fixture = Fixture::new();
    let inspector = fixture.inspector();
    let hidden = fixture.outside.join("20260105-000000-eeeeee");
    std::fs::create_dir_all(&hidden).unwrap();
    std::fs::create_dir_all(fixture.root.join(GROUPS_DIR)).unwrap();
    std::os::unix::fs::symlink(&hidden, fixture.group_dir("20260105-000000-eeeeee")).unwrap();

    // Then: ids that are not plain ids are refused outright.
    for id in ["../outside", "a/b", "", ".", ".."] {
        let found = inspector.group(id);
        assert_eq!(
            found.problem().map(|p| p.kind),
            Some(ProblemKind::UnsafePath),
            "{id:?}"
        );
    }
    // Then: a linked group directory is refused.
    assert_eq!(
        inspector
            .group("20260105-000000-eeeeee")
            .problem()
            .unwrap()
            .kind,
        ProblemKind::UnsafePath
    );

    // Given: a group whose record is a link to a file outside.
    let id = "20260106-000000-ffffff";
    let record = fixture.group(id, &[("thread-1", "member-1", None)]);
    let target = fixture.outside.join("group.json");
    std::fs::write(&target, serde_json::to_string(&record).unwrap()).unwrap();
    let path = fixture.group_dir(id).join(GROUP_FILE);
    std::fs::remove_file(&path).unwrap();
    std::os::unix::fs::symlink(&target, &path).unwrap();

    // Then: the record is not read through the link.
    assert_eq!(
        inspector.group(id).problem().unwrap().kind,
        ProblemKind::UnsafePath
    );

    // Given: a record that belongs to another group.
    let other = "20260107-000000-111111";
    fixture.group(other, &[("thread-1", "member-1", None)]);
    std::fs::create_dir_all(fixture.group_dir("20260108-000000-222222")).unwrap();
    std::fs::copy(
        fixture.group_dir(other).join(GROUP_FILE),
        fixture.group_dir("20260108-000000-222222").join(GROUP_FILE),
    )
    .unwrap();
    assert_eq!(
        inspector
            .group("20260108-000000-222222")
            .problem()
            .unwrap()
            .kind,
        ProblemKind::Malformed
    );
}

#[test]
fn member_and_trial_directories_are_confined_and_never_followed_through_links() {
    // Given: a member recorded outside the root, one that is a link to a sibling, and one
    // whose trial directory is a link to a planted outcome elsewhere.
    let fixture = Fixture::new();
    let planted = fixture.outside.join("planted");
    std::fs::create_dir_all(&planted).unwrap();
    output::write_json(
        &planted.join(output::MANIFEST_FILE),
        &manifest(
            &["baseline"],
            1,
            vec![run("baseline", 1, Some(Status::Completed))],
            true,
        ),
    )
    .unwrap();
    let mut record = fixture.group(
        GROUP,
        &[
            ("thread-1", "member-1", None),
            ("thread-2", "member-2", None),
            ("thread-3", "member-3", None),
        ],
    );
    record.members[0].dir = planted.clone();
    std::os::unix::fs::symlink(&planted, fixture.member_dir("member-2")).unwrap();
    let third = fixture.member_dir("member-3");
    std::fs::create_dir_all(third.join("runs")).unwrap();
    output::write_json(
        &third.join(output::MANIFEST_FILE),
        &manifest(
            &["baseline"],
            1,
            vec![run("baseline", 1, Some(Status::Completed))],
            true,
        ),
    )
    .unwrap();
    output::write_json(
        &planted.join(output::OUTCOME_FILE),
        &outcome(Status::Completed),
    )
    .unwrap();
    std::os::unix::fs::symlink(&planted, third.join("runs").join("baseline-1")).unwrap();
    output::write_json(&fixture.group_dir(GROUP).join(GROUP_FILE), &record).unwrap();

    // When / Then: no member reads the planted files.
    let group = fixture.inspector().group(GROUP).into_present().unwrap();
    for member in &group.members[..2] {
        assert_eq!(member.dir.problem().unwrap().kind, ProblemKind::UnsafePath);
        assert!(member.progress.is_none());
        assert!(
            member
                .trials
                .iter()
                .all(|trial| trial.state != TrialState::Finished)
        );
    }
    let trial = &group.members[2].trials[0];
    assert_eq!(trial.state, TrialState::Finished);
    assert!(
        trial
            .outcome
            .problem()
            .is_some_and(|p| p.kind == ProblemKind::UnsafePath)
    );
    assert!(trial.artifacts.as_ref().unwrap().problem().is_some());
}

#[test]
fn a_manifest_swapped_for_a_link_a_fifo_or_junk_is_reported_and_the_next_poll_recovers() {
    // Given: a launched member with a good manifest.
    let fixture = Fixture::new();
    let record = fixture.group(GROUP, &[("thread-1", "member-1", None)]);
    let (monitor, _live) = fixture.launched(record);
    let inspector = fixture.inspector().with_monitor(monitor);
    let dir = fixture.member_dir("member-1");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(output::MANIFEST_FILE);
    let good = |runs| manifest(&["baseline"], 1, runs, false);
    output::write_json(&path, &good(Vec::new())).unwrap();
    let kinds = |member: &EvaluationMemberSnapshot| -> Vec<ProblemKind> {
        member.problems.iter().map(|problem| problem.kind).collect()
    };
    let member = member_of(inspector.group(GROUP));
    assert_eq!(member.trials[0].state, TrialState::Queued);
    assert!(member.problems.is_empty());

    // When: replaced by an atomic write of a newer manifest.
    output::write_json(&path, &good(vec![run("baseline", 1, None)])).unwrap();

    // Then: one poll sees the new content.
    assert_eq!(
        member_of(inspector.group(GROUP)).trials[0].state,
        TrialState::Running
    );

    // When: replaced by a link to a file outside.
    let elsewhere = fixture.outside.join("manifest.json");
    output::write_json(
        &elsewhere,
        &good(vec![run("baseline", 1, Some(Status::Completed))]),
    )
    .unwrap();
    std::fs::remove_file(&path).unwrap();
    std::os::unix::fs::symlink(&elsewhere, &path).unwrap();

    // Then: the link is refused and the outside file is not believed.
    let member = member_of(inspector.group(GROUP));
    assert_eq!(kinds(&member), [ProblemKind::UnsafePath]);
    assert_ne!(member.trials[0].state, TrialState::Finished);

    // When: replaced by a FIFO nobody writes to.
    std::fs::remove_file(&path).unwrap();
    rustix::fs::mknodat(
        rustix::fs::CWD,
        &path,
        rustix::fs::FileType::Fifo,
        rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
        0,
    )
    .unwrap();

    // Then: reading it neither blocks nor succeeds.
    assert_eq!(
        kinds(&member_of(inspector.group(GROUP))),
        [ProblemKind::UnsafePath]
    );

    // When: replaced by a half-written file.
    std::fs::remove_file(&path).unwrap();
    std::fs::write(&path, "{\"thread_id\": \"thr").unwrap();

    // Then: it is a parse problem, and the member still has its planned trials.
    let member = member_of(inspector.group(GROUP));
    assert_eq!(kinds(&member), [ProblemKind::Malformed]);
    assert_eq!(member.phase, MemberPhase::Starting);
    assert_eq!(member.trials[0].state, TrialState::Queued);

    // When: a good manifest is written again, and then removed.
    output::write_json(
        &path,
        &good(vec![run("baseline", 1, Some(Status::Completed))]),
    )
    .unwrap();
    let member = member_of(inspector.group(GROUP));
    assert!(member.problems.is_empty());
    assert_eq!(member.trials[0].state, TrialState::Finished);
    std::fs::remove_file(&path).unwrap();

    // Then: a missing manifest is no problem.
    let member = member_of(inspector.group(GROUP));
    assert!(member.problems.is_empty());
    assert_eq!(member.phase, MemberPhase::Starting);
}

#[test]
fn oversized_files_and_huge_plans_are_bounded() {
    // Given: a manifest padded past the size bound, and one that plans billions of trials.
    let fixture = Fixture::new();
    fixture.group(
        GROUP,
        &[
            ("thread-1", "member-1", None),
            ("thread-2", "member-2", None),
        ],
    );
    let first = fixture.member_dir("member-1");
    std::fs::create_dir_all(&first).unwrap();
    let mut padded = manifest(&["baseline"], 1, Vec::new(), false);
    padded.problem = Some("x".repeat(20_000));
    output::write_json(&first.join(output::MANIFEST_FILE), &padded).unwrap();
    let second = fixture.member_dir("member-2");
    std::fs::create_dir_all(&second).unwrap();
    output::write_json(
        &second.join(output::MANIFEST_FILE),
        &manifest(&["baseline", "candidate"], u32::MAX, Vec::new(), false),
    )
    .unwrap();
    let limited = |limits: InspectionLimits| {
        fixture
            .inspector()
            .with_limits(limits)
            .group(GROUP)
            .into_present()
            .unwrap()
    };

    // When / Then: the large manifest is refused as too large.
    let group = limited(InspectionLimits {
        max_json_bytes: 10_000,
        ..InspectionLimits::default()
    });
    assert_eq!(group.members[0].problems[0].kind, ProblemKind::TooLarge);

    // When / Then: the huge plan lists a bounded number of trials and counts the rest.
    let group = limited(InspectionLimits {
        max_trials: 5,
        ..InspectionLimits::default()
    });
    let member = &group.members[1];
    assert_eq!(member.trials.len(), 5);
    assert_eq!(member.trials_omitted, 2 * u32::MAX as usize - 5);

    // When / Then: the member bound leaves members out and counts them.
    let group = limited(InspectionLimits {
        max_members: 1,
        ..InspectionLimits::default()
    });
    assert_eq!((group.members.len(), group.members_omitted), (1, 1));
}

#[test]
fn deleted_artifacts_and_unreadable_files_are_told_apart() {
    // Given: a finished trial whose directory was deleted, and one whose outcome is junk.
    let fixture = Fixture::new();
    fixture.group(
        GROUP,
        &[("thread-1", "member-1", Some(exit(Some(0), None, None)))],
    );
    let dir = fixture.member_dir("member-1");
    std::fs::create_dir_all(&dir).unwrap();
    let runs = vec![
        run("baseline", 1, Some(Status::Completed)),
        run("baseline", 2, Some(Status::Failed)),
    ];
    output::write_json(
        &dir.join(output::MANIFEST_FILE),
        &manifest(&["baseline"], 2, runs, true),
    )
    .unwrap();
    let second = write_trial(&dir, "baseline", 2, Status::Failed);
    std::fs::write(second.join(output::OUTCOME_FILE), "not json").unwrap();
    std::fs::write(second.join(output::DIFF_FILE), "diff").unwrap();

    // When / Then: the deleted one is missing, the junk one malformed.
    let member = member_of(fixture.inspector().group(GROUP));
    let first = &member.trials[0];
    assert_eq!(first.state, TrialState::Finished);
    assert!(first.outcome.is_missing());
    assert!(first.artifacts.as_ref().unwrap().is_missing());
    let second = &member.trials[1];
    assert_eq!(second.state, TrialState::Failed);
    assert_eq!(
        second.outcome.problem().unwrap().kind,
        ProblemKind::Malformed
    );
    let artifacts = second.artifacts.as_ref().unwrap().present().unwrap();
    assert_eq!(artifacts.diff.state, FileState::Present { bytes: 4 });
    assert_eq!(artifacts.database.state, FileState::Missing);

    // Given: a manifest the process may not read.
    if rustix::process::geteuid().is_root() {
        return;
    }
    let manifest_path = dir.join(output::MANIFEST_FILE);
    std::fs::set_permissions(
        &manifest_path,
        std::os::unix::fs::PermissionsExt::from_mode(0o000),
    )
    .unwrap();

    // Then: it is a permission problem, not a missing file.
    let member = member_of(fixture.inspector().group(GROUP));
    assert_eq!(member.problems[0].kind, ProblemKind::PermissionDenied);
}

#[test]
fn the_markdown_report_is_read_in_bounded_pieces() {
    // Given: a member with a report.md of multibyte text.
    let fixture = Fixture::new();
    fixture.group(GROUP, &[("thread-1", "member-1", None)]);
    let dir = fixture.member_dir("member-1");
    std::fs::create_dir_all(&dir).unwrap();
    let text = "評価".repeat(1_000);
    std::fs::write(dir.join(output::REPORT_MARKDOWN_FILE), &text).unwrap();
    let inspector = fixture.inspector();

    // When: it is read with a bound that cuts a character in two.
    let cut = inspector
        .member_report_markdown(GROUP, 0, 100)
        .into_present()
        .unwrap();

    // Then: the text ends on a character boundary and says it was cut.
    assert_eq!(cut.total_bytes, text.len() as u64);
    assert!(cut.truncated);
    assert_eq!(cut.text, format!("{}評", "評価".repeat(16)));
    assert!(text.starts_with(&cut.text));

    // Then: the configured maximum caps a larger request, and a small file is whole.
    let capped = fixture
        .inspector()
        .with_limits(InspectionLimits {
            max_markdown_bytes: 7,
            ..InspectionLimits::default()
        })
        .member_report_markdown(GROUP, 0, u64::MAX)
        .into_present()
        .unwrap();
    assert_eq!(capped.text, "評価");
    let whole = inspector
        .member_report_markdown(GROUP, 0, u64::MAX)
        .into_present()
        .unwrap();
    assert!(!whole.truncated && whole.text == text);

    // Then: a missing report.md, a missing report.json and an unknown member are missing.
    std::fs::remove_file(dir.join(output::REPORT_MARKDOWN_FILE)).unwrap();
    assert!(inspector.member_report_markdown(GROUP, 0, 10).is_missing());
    assert!(inspector.member_report(GROUP, 0).is_missing());
    assert!(inspector.member_report(GROUP, 5).is_missing());

    // Then: a present report.json is returned whole.
    output::write_json(
        &dir.join(output::REPORT_JSON_FILE),
        &report(&[("baseline", 1, 1.0)]),
    )
    .unwrap();
    assert_eq!(
        inspector
            .member_report(GROUP, 0)
            .present()
            .unwrap()
            .runs
            .len(),
        1
    );
}

fn tree(root: &Path) -> Vec<(PathBuf, u64, std::time::SystemTime)> {
    let mut found = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            let meta = std::fs::symlink_metadata(&path).unwrap();
            if meta.is_dir() {
                pending.push(path.clone());
            }
            found.push((path, meta.len(), meta.modified().unwrap()));
        }
    }
    found.sort();
    found
}

#[test]
fn inspecting_changes_nothing_on_disk() {
    // Given: a group with trials, a report and a failed neighbour.
    let fixture = Fixture::new();
    fixture.group(
        GROUP,
        &[
            ("thread-1", "member-1", Some(exit(Some(0), None, None))),
            ("thread-2", "member-2", Some(exit(Some(1), None, None))),
        ],
    );
    let dir = fixture.member_dir("member-1");
    std::fs::create_dir_all(&dir).unwrap();
    output::write_json(
        &dir.join(output::MANIFEST_FILE),
        &manifest(
            &["baseline"],
            1,
            vec![run("baseline", 1, Some(Status::Completed))],
            true,
        ),
    )
    .unwrap();
    write_trial(&dir, "baseline", 1, Status::Completed);
    output::write_json(
        &dir.join(output::REPORT_JSON_FILE),
        &report(&[("baseline", 1, 1.0)]),
    )
    .unwrap();
    std::fs::write(dir.join(output::REPORT_MARKDOWN_FILE), "# report").unwrap();
    let before = tree(&fixture.root);

    // When: everything the inspector offers is called.
    let inspector = fixture.inspector().with_monitor(LaunchMonitor::default());
    let _ = inspector.list_groups(10);
    let _ = inspector.group(GROUP);
    let _ = inspector.member_report(GROUP, 0);
    let _ = inspector.member_report_markdown(GROUP, 0, 100);

    // Then: no file was created, changed or removed.
    assert_eq!(tree(&fixture.root), before);
}

#[test]
fn group_phase_prefers_activity_then_doubt_then_failure() {
    use MemberPhase::{Failed, Finished, RunningTrials, Unverified};
    let phase = |phases: &[MemberPhase]| state::group_phase(phases, 0);
    assert_eq!(phase(&[Finished, RunningTrials]), GroupPhase::Running);
    assert_eq!(phase(&[Failed, Unverified]), GroupPhase::Unverified);
    assert_eq!(phase(&[Finished, Failed]), GroupPhase::Incomplete);
    assert_eq!(phase(&[Finished, Finished]), GroupPhase::Finished);
    assert_eq!(phase(&[]), GroupPhase::Incomplete);
    // Members nobody read can hide a running one, but not make the group look idle.
    assert_eq!(state::group_phase(&[Finished], 1), GroupPhase::Partial);
    assert_eq!(state::group_phase(&[RunningTrials], 1), GroupPhase::Running);
}

fn one_priced_row() -> Vec<storage::usage::UsageRequestRecord> {
    vec![usage("r1", "main", "gpt-test", "agent", 1_000_000)]
}

#[test]
fn a_directory_swapped_for_a_link_after_it_was_opened_is_still_read_from_the_opened_one() {
    // Given: a running member whose manifest is unfinished, and a planted
    // outside directory claiming everything finished.
    let fixture = Fixture::new();
    let record = fixture.group(GROUP, &[("thread-1", "member-1", None)]);
    let (monitor, _live) = fixture.launched(record);
    let member = fixture.member_dir("member-1");
    std::fs::create_dir_all(&member).unwrap();
    output::write_json(
        &member.join(output::MANIFEST_FILE),
        &manifest(&["baseline"], 1, vec![run("baseline", 1, None)], false),
    )
    .unwrap();
    let planted = fixture.outside.join("planted");
    std::fs::create_dir_all(&planted).unwrap();
    output::write_json(
        &planted.join(output::MANIFEST_FILE),
        &manifest(
            &["baseline"],
            1,
            vec![run("baseline", 1, Some(Status::Completed))],
            true,
        ),
    )
    .unwrap();
    let target = planted.clone();
    let _hook = on_open("opened_dir", "member-1", move |path| {
        replace_with_link(path, &target);
    });

    // When: the member directory becomes a link between being opened and being read.
    let member = member_of(fixture.inspector().with_monitor(monitor).group(GROUP));

    // Then: what was read is the directory that was opened.
    assert!(fixture.root.join("member-1.original").is_dir());
    assert_eq!(member.phase, MemberPhase::RunningTrials);
    assert_eq!(member.trials[0].state, TrialState::Running);
}

#[test]
fn a_trial_directory_swapped_after_it_was_opened_keeps_its_own_outcome() {
    // Given: a finished trial and a planted outcome elsewhere that says failed.
    let fixture = Fixture::new();
    fixture.group(
        GROUP,
        &[("thread-1", "member-1", Some(exit(Some(0), None, None)))],
    );
    let member = fixture.member_dir("member-1");
    std::fs::create_dir_all(&member).unwrap();
    output::write_json(
        &member.join(output::MANIFEST_FILE),
        &manifest(
            &["baseline"],
            1,
            vec![run("baseline", 1, Some(Status::Completed))],
            true,
        ),
    )
    .unwrap();
    write_trial(&member, "baseline", 1, Status::Completed);
    let planted = fixture.outside.join("planted-trial");
    std::fs::create_dir_all(&planted).unwrap();
    output::write_json(
        &planted.join(output::OUTCOME_FILE),
        &outcome(Status::Failed),
    )
    .unwrap();
    let target = planted.clone();
    let _hook = on_open("opened_dir", "baseline-1", move |path| {
        replace_with_link(path, &target);
    });

    // When / Then: the outcome is the one the trial directory had when opened.
    let member = member_of(fixture.inspector().group(GROUP));
    assert!(
        fixture
            .member_dir("member-1")
            .join("runs/baseline-1.original")
            .is_dir()
    );
    let found = member.trials[0].outcome.present().expect("an outcome");
    assert_eq!(found.status, Status::Completed);
}

#[test]
fn a_database_replaced_after_it_was_checked_is_not_the_one_that_is_priced() {
    // Given: a database with one priced request, and a different one planted outside.
    let fixture = Fixture::new();
    let trial = finished_trial_with_database(&fixture, one_priced_row());
    let planted = fixture.outside.join("planted.db");
    write_run_db(
        &planted,
        &[],
        vec![usage("r9", "main", "gpt-test", "agent", 9_000_000)],
    );
    let target = planted.clone();
    let _hook = on_open("opened_database", "evorch.db", move |path| {
        replace_with_link(path, &target);
    });

    // When: the path is swapped for a link between the check and the read.
    let cost = first_cost(&fixture.inspector().with_prices(prices()));

    // Then: the original database is priced (1M tokens at $2), not the planted one.
    let TrialCost::Estimated(cost) = cost else {
        panic!("{cost:?}");
    };
    assert_eq!(cost.usd, 2.0);
    assert!(trial.join("evorch.db.original").exists());
}

#[test]
fn reading_a_database_creates_no_side_files() {
    // Given: a finished trial with a database.
    let fixture = Fixture::new();
    finished_trial_with_database(&fixture, one_priced_row());
    let before = tree(&fixture.root);

    // When: its cost is estimated.
    let cost = first_cost(&fixture.inspector().with_prices(prices()));

    // Then: it is priced and the tree is untouched (no -wal, -shm or -journal).
    assert!(matches!(cost, TrialCost::Estimated(_)), "{cost:?}");
    assert_eq!(tree(&fixture.root), before);
}

#[test]
fn a_database_over_the_byte_or_row_budget_gives_no_cost_instead_of_a_partial_one() {
    // Given: a database with three priced requests.
    let fixture = Fixture::new();
    let rows = (1..=3)
        .map(|n| usage(&format!("r{n}"), "main", "gpt-test", "agent", 1_000_000))
        .collect();
    let trial = finished_trial_with_database(&fixture, rows);
    let size = std::fs::metadata(trial.join("evorch.db")).unwrap().len();
    let limited = |limits: InspectionLimits| {
        fixture
            .inspector()
            .with_prices(prices())
            .with_limits(limits)
    };

    // Then: at the row budget exactly, all three are priced.
    let exact = first_cost(&limited(InspectionLimits {
        max_database_rows: 3,
        ..InspectionLimits::default()
    }));
    let TrialCost::Estimated(exact) = exact else {
        panic!("{exact:?}");
    };
    assert_eq!(exact.usd, 6.0);

    // Then: one row over the budget gives no cost, and no member cost either.
    let inspector = limited(InspectionLimits {
        max_database_rows: 2,
        ..InspectionLimits::default()
    });
    assert_eq!(
        first_cost(&inspector),
        TrialCost::Unavailable(CostUnavailable::OverBudget(DatabaseBudget::Rows {
            limit: 2
        }))
    );
    assert_eq!(
        member_of(inspector.group(GROUP)).cost.source,
        CostSource::None
    );

    // Then: a file over the byte budget is not opened.
    let inspector = limited(InspectionLimits {
        max_database_bytes: size - 1,
        ..InspectionLimits::default()
    });
    assert_eq!(
        first_cost(&inspector),
        TrialCost::Unavailable(CostUnavailable::OverBudget(DatabaseBudget::Bytes {
            limit: size - 1,
            actual: size
        }))
    );
}

#[test]
fn a_database_with_pending_changes_next_to_it_is_not_priced() {
    // Given: a database with a non-empty write-ahead log beside it.
    let fixture = Fixture::new();
    let trial = finished_trial_with_database(&fixture, one_priced_row());
    std::fs::write(trial.join("evorch.db-wal"), vec![1_u8; 32]).unwrap();

    // When / Then: the file alone would miss the log, so there is no cost.
    let inspector = fixture.inspector().with_prices(prices());
    assert_eq!(
        first_cost(&inspector),
        TrialCost::Unavailable(CostUnavailable::InUse)
    );
}

#[test]
fn database_failures_are_classified_not_all_called_io() {
    // Given: a finished trial with a good database.
    let fixture = Fixture::new();
    let trial = finished_trial_with_database(&fixture, one_priced_row());
    let database = trial.join("evorch.db");
    let good = std::fs::read(&database).unwrap();
    let inspector = fixture.inspector().with_prices(prices());
    let problem = |cost: TrialCost| match cost {
        TrialCost::Unavailable(CostUnavailable::Unreadable(problem)) => problem.kind,
        other => panic!("{other:?}"),
    };

    // When / Then: a file that is not a database is malformed.
    std::fs::write(
        &database,
        "this is not a database, but it is long enough to look at",
    )
    .unwrap();
    assert_eq!(problem(first_cost(&inspector)), ProblemKind::Malformed);

    // When / Then: a schema newer than this build is an unsupported schema.
    let mut newer = good.clone();
    newer[60..64].copy_from_slice(&9_999_u32.to_be_bytes());
    std::fs::write(&database, &newer).unwrap();
    assert_eq!(
        problem(first_cost(&inspector)),
        ProblemKind::UnsupportedSchema
    );

    // When / Then: so is one older than this build, which only evorch itself may migrate.
    let mut older = good.clone();
    older[60..64].copy_from_slice(&1_u32.to_be_bytes());
    std::fs::write(&database, &older).unwrap();
    assert_eq!(
        problem(first_cost(&inspector)),
        ProblemKind::UnsupportedSchema
    );

    // When / Then: a database nobody may read is a permission problem.
    std::fs::write(&database, &good).unwrap();
    if !rustix::process::geteuid().is_root() {
        std::fs::set_permissions(
            &database,
            std::os::unix::fs::PermissionsExt::from_mode(0o000),
        )
        .unwrap();
        assert_eq!(
            problem(first_cost(&inspector)),
            ProblemKind::PermissionDenied
        );
        std::fs::set_permissions(
            &database,
            std::os::unix::fs::PermissionsExt::from_mode(0o644),
        )
        .unwrap();
    }

    // When / Then: a link is refused, and a deleted database is missing.
    let kept = fixture.outside.join("kept.db");
    std::fs::rename(&database, &kept).unwrap();
    std::os::unix::fs::symlink(&kept, &database).unwrap();
    assert_eq!(problem(first_cost(&inspector)), ProblemKind::UnsafePath);
    std::fs::remove_file(&database).unwrap();
    assert_eq!(
        first_cost(&inspector),
        TrialCost::Unavailable(CostUnavailable::NoDatabase)
    );
}

#[test]
fn an_unresolvable_root_is_a_problem_but_a_missing_one_is_just_empty() {
    // Given: an evaluation root inside a directory nobody may enter.
    let fixture = Fixture::new();
    let parent = fixture.outside.join("closed");
    std::fs::create_dir_all(parent.join("eval")).unwrap();
    let missing = EvaluationInspector::new(fixture.root.join("nowhere")).list_groups(10);
    assert!(missing.entries.is_empty() && missing.problem.is_none());
    if rustix::process::geteuid().is_root() {
        return;
    }
    std::fs::set_permissions(&parent, std::os::unix::fs::PermissionsExt::from_mode(0o000)).unwrap();

    // When: the root cannot be resolved for a reason other than absence.
    let inspector = EvaluationInspector::new(parent.join("eval"));
    let listing = inspector.list_groups(10);
    let group = inspector.group(GROUP);
    std::fs::set_permissions(&parent, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();

    // Then: the listing and the group say so, rather than showing nothing.
    assert!(listing.entries.is_empty());
    assert_eq!(
        listing.problem.map(|p| p.kind),
        Some(ProblemKind::PermissionDenied)
    );
    assert_eq!(
        group.problem().map(|p| p.kind),
        Some(ProblemKind::PermissionDenied)
    );
}

#[test]
fn members_left_out_of_the_listing_keep_the_group_from_looking_finished() {
    // Given: two members; the first is finished, the second still runs.
    let fixture = Fixture::new();
    let mut record = fixture.group(
        GROUP,
        &[
            ("thread-1", "member-1", Some(exit(Some(0), None, None))),
            ("thread-2", "member-2", None),
        ],
    );
    let first = fixture.member_dir("member-1");
    std::fs::create_dir_all(&first).unwrap();
    output::write_json(
        &first.join(output::MANIFEST_FILE),
        &manifest(
            &["baseline"],
            1,
            vec![run("baseline", 1, Some(Status::Completed))],
            true,
        ),
    )
    .unwrap();
    write_trial(&first, "baseline", 1, Status::Completed);
    output::write_json(
        &first.join(output::REPORT_JSON_FILE),
        &report(&[("baseline", 1, 1.0)]),
    )
    .unwrap();
    record.members[0].exit = Some(exit(Some(0), None, None));
    let (monitor, _live) = fixture.launched(record);
    let inspector = fixture.inspector().with_monitor(monitor);

    // When: read in full, and with the listing cut after the first member.
    let full = inspector.group(GROUP).into_present().unwrap();
    let cut = fixture
        .inspector()
        .with_monitor(inspector.monitor.clone().unwrap())
        .with_limits(InspectionLimits {
            max_members: 1,
            ..InspectionLimits::default()
        })
        .group(GROUP)
        .into_present()
        .unwrap();

    // Then: the whole group runs; the cut one cannot say it finished.
    assert_eq!(full.phase, GroupPhase::Running);
    assert_eq!(cut.members[0].phase, MemberPhase::Finished);
    assert_eq!(cut.phase, GroupPhase::Partial);
    assert_eq!(
        (cut.members.len(), cut.members_omitted, cut.members_total),
        (1, 1, 2)
    );
    assert!(cut.finished_at.is_none());
}

#[test]
fn trials_left_out_of_the_listing_still_count_and_keep_the_cost_partial() {
    // Given: a launched member with two baseline repeats, the first finished with a
    // priced database, the second still running.
    let fixture = Fixture::new();
    let record = fixture.group(GROUP, &[("thread-1", "member-1", None)]);
    let (monitor, _live) = fixture.launched(record);
    let member = fixture.member_dir("member-1");
    std::fs::create_dir_all(&member).unwrap();
    let runs = vec![
        run("baseline", 1, Some(Status::Completed)),
        run("baseline", 2, None),
    ];
    output::write_json(
        &member.join(output::MANIFEST_FILE),
        &manifest(&["baseline"], 2, runs, false),
    )
    .unwrap();
    let first = write_trial(&member, "baseline", 1, Status::Completed);
    write_run_db(&first.join(output::DATABASE_FILE), &[], one_priced_row());

    // When: only the first trial is listed.
    let member = member_of(
        fixture
            .inspector()
            .with_monitor(monitor)
            .with_prices(prices())
            .with_limits(InspectionLimits {
                max_trials: 1,
                ..InspectionLimits::default()
            })
            .group(GROUP),
    );

    // Then: the counts cover both trials, and the cost says it covers one of two.
    assert_eq!(member.trials.len(), 1);
    assert_eq!(member.trials_omitted, 1);
    assert_eq!(
        (
            member.tally.total,
            member.tally.finished,
            member.tally.running
        ),
        (2, 1, 1)
    );
    assert_eq!(member.finished_trials(), 1);
    assert_eq!(member.planned_trials(), 2);
    let [baseline] = member.cost.configurations.as_slice() else {
        panic!("{:?}", member.cost);
    };
    assert_eq!((baseline.counted, baseline.planned), (1, 2));
    assert!(baseline.is_partial());
}
