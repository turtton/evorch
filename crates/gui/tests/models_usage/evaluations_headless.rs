use crate::evaluations_support;

use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

use eval::inspection::{
    Artifact, ArtifactProblem, GroupListEntry, GroupPhase, MemberPhase, ProblemKind, Purpose,
    ReportStatus, TrialCost, TrialState,
};
use evaluations_support::{
    FakeReader, MemberSpec, TrialSpec, available_report, entry, estimated, group, listing, member,
    no_cost, trial, unavailable_cost,
};
use event_bus::event::diagnostic_codes;
use event_bus::{DiagnosticEvent, DiagnosticSeverity, Event};
use gui::app::WorkbenchState;
use gui::fixture::DemoSource;
use gui::headless::HeadlessWorkbench;
use gui::model::evaluations::{
    EvaluationsModel, Loaded, MAX_IN_FLIGHT, ManualSpawner, RUNNING_GROUP_POLL, thread_spawner,
};
use workspace_ui::UiSettings;

fn panel(
    reader: Arc<FakeReader>,
    spawner: gui::model::evaluations::Spawner,
) -> HeadlessWorkbench<DemoSource> {
    let model = EvaluationsModel::new(Some(reader), spawner, Arc::new(|| {}));
    let state = WorkbenchState::new(DemoSource(Vec::new()), &UiSettings::default())
        .expect("default state builds")
        .with_evaluations(model);
    let mut harness = HeadlessWorkbench::new(state, [1200.0, 800.0]);
    harness.state_mut().open_evaluations_tab();
    harness
}

/// Frames, then every read they started, until no read is left.
fn settle(harness: &mut HeadlessWorkbench<DemoSource>, reads: &ManualSpawner) {
    for _ in 0..8 {
        harness.run();
        if reads.pending() == 0 {
            return;
        }
        reads.run_all();
    }
    harness.run();
}

fn two_thread_group() -> eval::inspection::EvaluationGroupSnapshot {
    let target = member(MemberSpec {
        index: 0,
        thread: "thread-target",
        purpose: Purpose::Target,
        phase: MemberPhase::RunningTrials,
        trials: vec![
            trial(TrialSpec {
                label: "baseline",
                index: 1,
                state: TrialState::Finished,
                cost: TrialCost::Estimated(evaluations_support::cost(0.25)),
                database: None,
            }),
            trial(TrialSpec {
                label: "candidate",
                index: 1,
                state: TrialState::Running,
                cost: unavailable_cost(),
                database: None,
            }),
        ],
        planned: 2,
        report: ReportStatus::Pending,
        markdown: false,
        cost: estimated(0.25, 1, 2),
    });
    let regression = member(MemberSpec {
        index: 1,
        thread: "thread-regress",
        purpose: Purpose::Regression,
        phase: MemberPhase::Finished,
        trials: vec![trial(TrialSpec {
            label: "baseline",
            index: 1,
            state: TrialState::Finished,
            cost: TrialCost::Reported(evaluations_support::cost(1.5)),
            database: None,
        })],
        planned: 1,
        report: available_report(),
        markdown: true,
        cost: no_cost(),
    });
    group("g-main", GroupPhase::Running, vec![target, regression])
}

#[test]
fn a_group_opens_to_its_threads_trials_progress_and_report() {
    // Given: one running group with a thread in progress and a thread with a report.
    let reader = FakeReader::new(listing(vec![entry(two_thread_group())]))
        .with_group(two_thread_group())
        .with_markdown(
            "g-main",
            1,
            "# Findings\n\nSee [the docs](https://example.invalid/docs).",
        );
    let reads = ManualSpawner::default();
    let mut harness = panel(Arc::clone(&reader), reads.spawner());

    // When: the user walks group -> thread.
    settle(&mut harness, &reads);
    assert!(harness.has_label("g-main"), "the group is listed");
    harness.click_label("g-main");
    settle(&mut harness, &reads);
    assert!(harness.has_label("target · thread-target"));
    assert!(harness.has_label("regression · thread-regress"));
    harness.click_label("target · thread-target");
    settle(&mut harness, &reads);

    // Then: progress is trials finished over planned, never a percentage.
    assert!(harness.has_label("1 of 2 trials finished, 2 started"));
    // And: each trial shows its state and cost; an unfinished trial's cost is a dash.
    assert!(harness.has_label("baseline-1"));
    assert!(harness.has_label("candidate-1"));
    assert!(harness.has_label("cost —"));
    // And: a replay is offered for the finished trial only.
    assert!(harness.has_label("Replay baseline-1"));
    assert!(!harness.has_label("Replay candidate-1"));

    // When: the thread with a report is opened.
    let state = harness.state_mut();
    state.evaluations_mut().clear_member();
    state.evaluations_mut().select_member(1);
    settle(&mut harness, &reads);

    // Then: report.md is read through the bounded reader and drawn with links as plain text.
    let model = harness.state().evaluations();
    let Some(Loaded::Ready(report)) = model.markdown() else {
        panic!("report.md should be loaded, got {:?}", model.markdown());
    };
    assert!(report.text.contains("Findings"));
    assert!(
        !report.text.contains("](https://"),
        "a report link must not stay a live link: {}",
        report.text
    );
}

#[test]
fn partial_missing_and_unreadable_records_are_named_not_hidden() {
    // Given: a listing with a Partial group, an unreadable record and a vanished one.
    let problem = ArtifactProblem::new(
        ProblemKind::Malformed,
        std::path::Path::new("groups/g-bad/group.json"),
        "expected value",
    );
    let partial = group("g-partial", GroupPhase::Partial, Vec::new());
    let listing = listing(vec![
        entry(partial),
        GroupListEntry {
            id: "g-bad".into(),
            group: Artifact::Problem(problem.clone()),
        },
        GroupListEntry {
            id: "g-gone".into(),
            group: Artifact::Missing,
        },
    ]);
    let reads = ManualSpawner::default();
    let mut harness = panel(FakeReader::new(listing), reads.spawner());

    // When: the panel lists them.
    settle(&mut harness, &reads);

    // Then: each is labelled with what is wrong.
    assert!(harness.has_label("Partial"));
    assert!(harness.has_label(&format!("Artifact problem: {problem}")));
    assert!(harness.has_label("record missing"));
}

#[test]
fn a_cost_that_cannot_be_read_is_a_dash_and_a_partial_total_says_so() {
    // Given: a thread whose trial cost is unavailable and whose total counts one of three trials.
    let only = member(MemberSpec {
        index: 0,
        thread: "thread-a",
        purpose: Purpose::Target,
        phase: MemberPhase::RunningTrials,
        trials: vec![trial(TrialSpec {
            label: "baseline",
            index: 1,
            state: TrialState::Finished,
            cost: TrialCost::Unavailable(eval::inspection::CostUnavailable::InUse),
            database: None,
        })],
        planned: 3,
        report: ReportStatus::Pending,
        markdown: false,
        cost: estimated(0.5, 1, 3),
    });
    let snapshot = group("g-cost", GroupPhase::Running, vec![only]);
    let reader = FakeReader::new(listing(vec![entry(snapshot.clone())])).with_group(snapshot);
    let reads = ManualSpawner::default();
    let mut harness = panel(reader, reads.spawner());

    // When: the thread is opened.
    settle(&mut harness, &reads);
    harness.click_label("g-cost");
    settle(&mut harness, &reads);
    harness.click_label("target · thread-a");
    settle(&mut harness, &reads);

    // Then: no zero is shown for the unknown cost, and the partial total is marked.
    assert!(harness.has_label("cost —"));
    assert!(harness.has_label("baseline: $0.5000 — counted trials only (1 of 3)"));
}

#[test]
fn an_evaluation_finished_notice_opens_the_panel_and_reads_again_without_running_anything() {
    // Given: a workbench whose Evaluations panel was never opened.
    let reader = FakeReader::new(listing(vec![entry(two_thread_group())]));
    let reads = ManualSpawner::default();
    let model = EvaluationsModel::new(Some(reader.clone()), reads.spawner(), Arc::new(|| {}));
    let state = WorkbenchState::new(DemoSource(Vec::new()), &UiSettings::default())
        .expect("default state builds")
        .with_evaluations(model);
    let mut harness = HeadlessWorkbench::new(state, [1200.0, 800.0]);
    harness.run();
    assert_eq!(
        reads.pending(),
        0,
        "a panel that is not shown reads nothing"
    );

    // When: the Evaluator reports a finished evaluation and the user follows the notice.
    harness
        .state_mut()
        .apply_events([Event::new(DiagnosticEvent {
            source: "eval".into(),
            severity: DiagnosticSeverity::Info,
            code: diagnostic_codes::EVALUATION_FINISHED.into(),
            detail: "free text that names no group".into(),
            run_id: Some("run-1".into()),
            thread_id: None,
            call_id: None,
        })]);
    harness.run();
    let notice = harness
        .state()
        .notifications()
        .items()
        .next()
        .expect("the notice is recorded")
        .clone();
    assert!(matches!(
        notice.kind,
        gui::model::notifications::NotificationKind::EvaluationFinished { all_reported: true }
    ));
    let notifications = workspace_ui::PanelId::new("notifications-main");
    let tab = harness
        .state()
        .dock()
        .find_tab(&notifications)
        .expect("the notifications tab is docked");
    harness
        .state_mut()
        .dock_mut()
        .set_active_tab(tab)
        .expect("the tab can be shown");
    harness.run();
    harness.click_label("Open evaluations");
    settle(&mut harness, &reads);

    // Then: the panel shows the group list, read afresh, and nothing but reads happened.
    assert!(harness.has_label("g-main"));
    assert_eq!(
        reader.list_calls.load(std::sync::atomic::Ordering::SeqCst),
        1
    );
}

#[test]
fn a_late_result_for_a_group_that_is_no_longer_selected_is_dropped() {
    // Given: two groups and reads that finish in the opposite order they started.
    let first = group("g-first", GroupPhase::Finished, Vec::new());
    let second = group("g-second", GroupPhase::Finished, Vec::new());
    let reader = FakeReader::new(listing(vec![entry(first.clone()), entry(second.clone())]))
        .with_group(first)
        .with_group(second);
    let reads = ManualSpawner::default();
    let mut model = EvaluationsModel::new(Some(reader), reads.spawner(), Arc::new(|| {}));
    let t0 = Instant::now();
    model.poll_once(t0, true);
    reads.run_all();
    model.poll_once(t0, true);

    // When: the user selects one group and then the other before either read ends.
    model.select_group("g-first");
    model.poll_once(t0, true);
    model.select_group("g-second");
    model.poll_once(t0, true);
    assert_eq!(reads.pending(), 2);
    // The newer read ends first, the stale one afterwards.
    assert!(reads.run_newest());
    model.poll_once(t0, true);
    assert!(reads.run_newest());
    model.poll_once(t0, true);

    // Then: only the selected group is shown.
    let shown = model
        .group()
        .and_then(Artifact::present)
        .map(|group| group.id.clone());
    assert_eq!(shown.as_deref(), Some("g-second"));
}

#[test]
fn polling_follows_a_running_evaluation_and_stops_when_hidden() {
    // Given: a running group that is open.
    let running = group("g-run", GroupPhase::Running, Vec::new());
    let reader = FakeReader::new(listing(vec![entry(running.clone())])).with_group(running);
    let reads = ManualSpawner::default();
    let mut model = EvaluationsModel::new(Some(reader.clone()), reads.spawner(), Arc::new(|| {}));
    let t0 = Instant::now();
    model.poll_once(t0, true);
    reads.run_all();
    model.poll_once(t0, true);
    model.select_group("g-run");
    model.poll_once(t0, true);
    reads.run_all();
    model.poll_once(t0, true);

    // When: less than a second passes, nothing is read; a second later the group is read again.
    let early = model.poll_once(t0 + Duration::from_millis(400), true);
    assert_eq!(reads.pending(), 0);
    assert_eq!(
        early.repaint_after,
        Some(RUNNING_GROUP_POLL - Duration::from_millis(400))
    );
    model.poll_once(t0 + RUNNING_GROUP_POLL, true);
    assert_eq!(reads.pending(), 1);
    reads.run_all();
    model.poll_once(t0 + RUNNING_GROUP_POLL, true);

    // Then: a hidden panel starts nothing and asks for no repaint, however late it is.
    let hidden = model.poll_once(t0 + Duration::from_secs(600), false);
    assert_eq!(reads.pending(), 0);
    assert_eq!(hidden.repaint_after, None);
    // And: showing it again reads at once.
    model.poll_once(t0 + Duration::from_secs(601), true);
    assert!(reads.pending() > 0);
}

#[test]
fn reads_in_flight_stay_bounded_however_often_the_user_asks() {
    // Given: reads that never finish.
    let reader = FakeReader::new(listing(Vec::new()));
    let reads = ManualSpawner::default();
    let mut model = EvaluationsModel::new(Some(reader), reads.spawner(), Arc::new(|| {}));
    let t0 = Instant::now();

    // When: the user refreshes and switches groups over and over.
    for round in 0..20 {
        model.request_refresh();
        model.select_group(&format!("g-{round}"));
        model.poll_once(t0, true);
    }

    // Then: the number of unfinished reads never passes the limit.
    assert!(model.in_flight() <= MAX_IN_FLIGHT, "{}", model.in_flight());
}

#[test]
fn the_ui_keeps_working_while_a_reader_is_stopped() {
    // Given: a reader that stops inside its first listing, on a real reader thread.
    let reader = FakeReader::new(listing(vec![entry(two_thread_group())]));
    let entered = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    *reader.gate.lock().expect("gate") = Some((Arc::clone(&entered), Arc::clone(&release)));
    let mut harness = panel(Arc::clone(&reader), thread_spawner());
    harness.run();
    entered.wait();

    // When: frames are drawn and the user clicks while the read is stopped.
    harness.run();
    harness.click_label("Refresh");
    harness.run();

    // Then: the panel shows that it is loading, and no second read was started.
    assert!(harness.has_label("Loading evaluations…"));
    assert_eq!(
        reader.list_calls.load(std::sync::atomic::Ordering::SeqCst),
        1
    );

    // When: the reader is let go and its result is taken in.
    release.wait();
    assert!(
        harness
            .state_mut()
            .evaluations_mut()
            .wait_for_result(Instant::now())
    );
    harness.run();

    // Then: the list appears.
    assert!(harness.has_label("g-main"));
}

#[test]
fn an_overdue_read_waiting_for_a_free_slot_does_not_spin_the_repaint_loop() {
    // Given: a listing that was read once, and then every read slot taken by reads
    // of groups that were selected one after another and have not finished.
    let reader = FakeReader::new(listing(Vec::new()));
    let reads = ManualSpawner::default();
    let mut model = EvaluationsModel::new(Some(reader), reads.spawner(), Arc::new(|| {}));
    let t0 = Instant::now();
    model.poll_once(t0, true);
    reads.run_all();
    model.poll_once(t0, true);
    for round in 0..MAX_IN_FLIGHT {
        model.select_group(&format!("g-{round}"));
        model.poll_once(t0, true);
    }
    assert_eq!(model.in_flight(), MAX_IN_FLIGHT);

    // When: the listing falls due while no slot is free.
    let later = t0 + Duration::from_secs(60);
    let poll = model.poll_once(later, true);

    // Then: nothing starts and no immediate repaint is requested.
    assert_eq!(model.in_flight(), MAX_IN_FLIGHT);
    assert_eq!(poll.repaint_after, None);

    // And: once a read finishes, the overdue listing is started.
    assert!(reads.run_next());
    model.poll_once(later, true);
    assert_eq!(reads.pending(), MAX_IN_FLIGHT);
}
