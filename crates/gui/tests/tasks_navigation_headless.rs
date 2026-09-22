use event_bus::{AgentRunPhase, Event, MessageEvent, OrchestratorEvent};
use gui::{app::WorkbenchState, fixture::DemoSource, headless::HeadlessWorkbench};
use runtime::{AgentSummary, RunId};
use workspace_ui::{PanelId, UiSettings};

fn summary(id: u64, name: &str) -> AgentSummary {
    AgentSummary {
        run_id: RunId::new(id),
        parent_run_id: Some(RunId::new(0)),
        name: name.into(),
        role_name: "worker".into(),
        phase: AgentRunPhase::Running,
        model: "fixture-model".into(),
    }
}

fn progress() -> Event {
    Event::new(OrchestratorEvent::TaskProgressed {
        task_id: "ship-ui".into(),
        run_id: "run-1".into(),
        progress: serde_json::json!({"status":"running", "last_artifact":"artifacts/layout.png"}),
        reason: "Implement the task view".into(),
    })
}

fn retry() -> Event {
    Event::new(OrchestratorEvent::TaskRetryScheduled {
        goal_id: "goal-ui".into(),
        task_id: "ship-ui".into(),
        attempt: 1,
        reason: "Retry after provider failure".into(),
        new_run_id: "run-2".into(),
    })
}

fn activate<S: gui::model::tasks::AgentRunSource>(state: &mut WorkbenchState<S>, id: &str) {
    let path = state.dock().find_tab(&PanelId::new(id)).expect("panel");
    state.dock_mut().set_active_tab(path).expect("activate");
}

fn capture<S: gui::model::tasks::AgentRunSource + 'static>(
    workbench: &mut HeadlessWorkbench<S>,
    name: &str,
) {
    if let Some(path) = std::env::var_os("EVORCH_TASKS_NAV_EVIDENCE") {
        let path = std::path::PathBuf::from(path);
        std::fs::create_dir_all(&path).expect("evidence directory");
        workbench
            .capture()
            .expect("offscreen frame")
            .save_png(&path.join(format!("{name}.png")))
            .expect("save evidence");
    }
}

#[test]
fn agents_open_related_task_and_task_opens_each_retry_run() {
    // A task keeps its identity across two attempts; only the current worker is listed.
    let mut state = WorkbenchState::new(
        DemoSource(vec![summary(2, "current-worker")]),
        &UiSettings::default(),
    )
    .expect("workbench");
    state.apply_events([
        progress(),
        retry(),
        Event::new(MessageEvent::MessageDelta {
            delta: "First attempt output".into(),
            run_id: Some("run-1".into()),
        }),
        Event::new(MessageEvent::MessageDelta {
            delta: "Retry attempt output".into(),
            run_id: Some("run-2".into()),
        }),
    ]);
    let mut workbench = HeadlessWorkbench::new(state, [1600.0, 1000.0]);
    workbench.run();
    assert!(workbench.has_label("current-worker"));
    assert!(workbench.has_label("Task: ship-ui"));
    capture(&mut workbench, "agents");

    workbench.click_label("Task: ship-ui");
    workbench.run();
    assert!(workbench.has_label("retrying"));
    assert!(workbench.has_label("retry 1"));
    assert!(workbench.has_label("artifacts/layout.png"));
    assert!(!workbench.has_label("current-worker"));
    assert!(
        workbench
            .state()
            .dock()
            .find_tab(&PanelId::new("durable-tasks-main"))
            .is_none()
    );
    capture(&mut workbench, "tasks");

    // Both the previous attempt and the current attempt have separate logs.
    for (run, output, other_output) in [
        ("run-1", "First attempt output", "Retry attempt output"),
        ("run-2", "Retry attempt output", "First attempt output"),
    ] {
        workbench.click_label(run);
        workbench.run();
        assert!(
            workbench
                .state()
                .dock()
                .find_tab(&PanelId::new(format!("agent-{run}")))
                .is_some()
        );
        assert!(workbench.has_label(output));
        assert!(!workbench.has_label(other_output));
    }
}

#[test]
fn agents_keep_task_links_for_previous_attempts() {
    let mut state = WorkbenchState::new(
        DemoSource(vec![summary(1, "previous-worker")]),
        &UiSettings::default(),
    )
    .expect("workbench");
    state.apply_events([progress(), retry()]);
    let mut workbench = HeadlessWorkbench::new(state, [1280.0, 900.0]);
    workbench.run();
    workbench.click_label("Task: ship-ui");
    workbench.run();
    assert!(workbench.has_label("retrying"));
    assert!(workbench.has_label("retry 1"));
    assert!(workbench.has_label("run-2"));
}

#[test]
fn ordinary_agent_runs_are_not_listed_as_tasks() {
    let mut state = WorkbenchState::new(
        DemoSource(vec![summary(1, "ordinary-worker")]),
        &UiSettings::default(),
    )
    .expect("workbench");
    let dir = tempfile::tempdir().expect("storage directory");
    let config = storage::StorageConfig {
        db_path: dir.path().join("tasks.db"),
        ..Default::default()
    };
    let storage = storage::Storage::open(config.clone()).expect("storage");
    let event = Event::new(event_bus::LifecycleEvent::BackgroundTaskStarted {
        task_id: "run-1".into(),
    });
    storage
        .handle()
        .append_event(None, &event)
        .expect("persist execution");
    storage.handle().reconcile().expect("project execution");
    state = state.with_memory_storage(config);
    state.apply_events([event]);
    activate(&mut state, "tasks-main");
    let mut workbench = HeadlessWorkbench::new(state, [1280.0, 900.0]);
    workbench.run();
    assert!(!workbench.has_label("ordinary-worker"));
    assert!(!workbench.has_label("run-1"));
    assert!(workbench.has_label("No tasks yet"));
}

#[test]
fn reopening_same_task_reveals_it_after_manual_scroll() {
    let mut state = WorkbenchState::new(
        DemoSource(vec![summary(1, "worker")]),
        &UiSettings::default(),
    )
    .expect("workbench");
    state.apply_events([progress()]);
    state.apply_events((0..30).map(|index| {
        Event::new(OrchestratorEvent::TaskProgressed {
            task_id: format!("earlier-{index:02}"),
            run_id: format!("run-{}", index + 10),
            progress: serde_json::json!({"status":"queued"}),
            reason: "Waiting for work".into(),
        })
    }));
    let mut workbench = HeadlessWorkbench::new(state, [1280.0, 1000.0]);
    workbench.run();
    workbench.click_label("Task: ship-ui");
    workbench.run();
    assert!(workbench.label_rects("ship-ui")[0].top() < 1000.0);
    workbench.scroll_label_into_view("earlier-00");
    workbench.run();
    assert!(workbench.label_rects("ship-ui")[0].top() > 1000.0);
    activate(workbench.state_mut(), "agents-main");
    workbench.run();
    workbench.click_label("Task: ship-ui");
    workbench.run();
    assert!(
        workbench.label_rects("ship-ui")[0].top() < 1000.0,
        "navigating again must reveal the selected task"
    );
}

#[test]
fn agents_link_to_claimed_team_task_and_task_opens_owner() {
    struct TeamSource(Vec<runtime::team::TeamTask>);
    impl gui::model::tasks::AgentRunSource for TeamSource {
        fn list(&self) -> Vec<AgentSummary> {
            vec![summary(3, "team-worker")]
        }
        fn teams(&self) -> Vec<(RunId, Vec<runtime::team::TeamTask>)> {
            vec![(RunId::new(1), self.0.clone())]
        }
    }
    let board = runtime::team::TeamBoard::default();
    board
        .enqueue(runtime::team::TaskSpec {
            id: "team-layout".into(),
            paths: vec!["src/gui".into()],
        })
        .expect("team task");
    board.claim("team-layout", "run-3", 0).expect("claim");
    let state = WorkbenchState::new(
        TeamSource(board.snapshot().unwrap()),
        &UiSettings::default(),
    )
    .expect("workbench");
    let mut workbench = HeadlessWorkbench::new(state, [1280.0, 1000.0]);
    workbench.run();
    workbench.click_label("Team task: team-layout");
    workbench.run();
    assert!(workbench.has_label("team-layout"));
    assert!(workbench.has_label("Claimed"));
    workbench.click_label("run-3");
    workbench.run();
    assert!(
        workbench
            .state()
            .dock()
            .find_tab(&PanelId::new("agent-run-3"))
            .is_some()
    );
}
