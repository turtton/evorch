use event_bus::{
    Event, OrchestratorEvent, ThreadGoalCheck, ThreadGoalPhase, ThreadGoalSnapshot, ThreadGoalUsage,
};
use gui::{
    app::WorkbenchState, fixture::DemoSource, headless::HeadlessWorkbench,
    model::commands::WorkbenchCommand,
};
use workspace_ui::{PanelId, ProjectId, SidebarState, ThreadId, UiSettings};

fn goal(thread: &str) -> ThreadGoalSnapshot {
    ThreadGoalSnapshot {
        related_root_run_ids: Vec::new(),
        max_tokens: None,
        goal_id: format!("goal-{thread}"),
        thread_id: thread.into(),
        root_run_id: if thread == "A" { "run-1" } else { "run-2" }.into(),
        objective: format!(
            "Investigate the cause of a slow operation in thread {thread}, document the findings and validate the proposed solution"
        ),
        original_request: "Explain the cause and show supporting observations".into(),
        criteria: vec!["Cause supported by observations".into()],
        checks: vec![ThreadGoalCheck {
            criterion: 0,
            met: true,
            evidence: "Profiling report: artifacts/profile.txt".into(),
        }],
        phase: ThreadGoalPhase::Working,
        review_enabled: false,
        checks_paused: false,
        work_stopped: false,
        epoch: 1,
        review_round: 0,
        findings: vec![],
        reason: None,
        usage: ThreadGoalUsage::default(),
        max_review_rounds: 3,
    }
}
fn event(snapshot: ThreadGoalSnapshot) -> Event {
    Event::new(OrchestratorEvent::ThreadGoalUpdated { snapshot })
}
fn bare_state(root: &std::path::Path) -> WorkbenchState<DemoSource> {
    let mut sidebar = SidebarState::default();
    let project = ProjectId::new("project");
    sidebar
        .add_project(project.clone(), "project", root)
        .unwrap();
    sidebar.select_project(&project).unwrap();
    for id in ["A", "B"] {
        sidebar
            .create_thread(ThreadId::new(id), project.clone(), id)
            .unwrap();
    }
    sidebar.switch_thread(&ThreadId::new("A")).unwrap();
    let mut state = WorkbenchState::new(DemoSource(vec![]), &UiSettings::default())
        .unwrap()
        .with_sidebar(sidebar);
    let path = state.dock().find_tab(&PanelId::new("agent-main")).unwrap();
    state.dock_mut().set_active_tab(path).unwrap();
    state
}

fn state(root: &std::path::Path) -> WorkbenchState<DemoSource> {
    let mut state = bare_state(root);
    state.apply_events([event(goal("A")), event(goal("B"))]);
    state
}
#[test]
fn controls_are_named_single_line_and_scoped_to_the_selected_thread() {
    let dir = tempfile::tempdir().unwrap();
    let mut workbench = HeadlessWorkbench::new(state(dir.path()), [1000.0, 800.0]);
    workbench.run();
    let a = goal("A");
    let label = format!("Goal: {}", a.objective);
    let title = workbench.label_rects(&label)[0];
    let controls = [
        "Enable independent review before completion",
        "Pause goal checks",
        "Show goal details",
    ];
    for control in controls {
        let rect = workbench.label_rects(control)[0];
        assert!((rect.center().y - title.center().y).abs() < 2.0);
        assert!(rect.right() <= workbench.screen_rect().right());
    }
    assert!(title.bottom() <= workbench.label_rects("Message or /command")[0].top());
    assert!(!workbench.has_label(&format!("Goal: {}", goal("B").objective)));
    workbench.click_label("Enable independent review before completion");
    workbench.run();
    assert!(
        matches!(workbench.state().issued().last(), Some(WorkbenchCommand::SetGoalReview { thread_id, goal_id, enabled: true }) if thread_id == "A" && goal_id == "goal-A")
    );
    workbench.click_label("Pause goal checks");
    workbench.run();
    assert!(
        matches!(workbench.state().issued().last(), Some(WorkbenchCommand::SetGoalChecksPaused { thread_id, paused: true, .. }) if thread_id == "A")
    );
    let mut paused = a;
    paused.checks_paused = true;
    paused.review_enabled = true;
    workbench.state_mut().apply_events([event(paused)]);
    workbench.run();
    assert!(workbench.has_label("Resume goal checks"));
    workbench
        .state_mut()
        .switch_thread(ThreadId::new("B"))
        .unwrap();
    workbench.run();
    assert!(workbench.has_label("Pause goal checks"));
    assert!(workbench.has_label("Enable independent review before completion"));
    workbench.click_label("Show goal details");
    workbench.run();
    assert!(
        workbench.has_label("Original request: Explain the cause and show supporting observations")
    );
    assert!(workbench.has_label("Profiling report: artifacts/profile.txt"));
    let original = workbench
        .label_rects("Original request: Explain the cause and show supporting observations")[0];
    assert!(original.bottom() < workbench.label_rects("Message or /command")[0].top());
    assert!(original.left() < title.right());
}

#[test]
fn replay_restores_thread_goal_controls_without_creating_work() {
    let dir = tempfile::tempdir().unwrap();
    let config = storage::StorageConfig {
        db_path: dir.path().join("events.sqlite"),
        ..Default::default()
    };
    let storage = storage::Storage::open(config.clone()).unwrap();
    let mut snapshot = goal("A");
    snapshot.checks_paused = true;
    snapshot.review_enabled = true;
    storage
        .handle()
        .append_event(None, &event(snapshot))
        .unwrap();
    let mut state = state(dir.path());
    state
        .restore_history(&storage::Database::open(&config).unwrap())
        .unwrap();
    assert!(state.issued().is_empty());
    let mut workbench = HeadlessWorkbench::new(state, [1000.0, 800.0]);
    workbench.run();
    assert!(workbench.has_label("Resume goal checks"));
    assert!(workbench.has_label("Disable independent review before completion"));
    storage.close();
}

#[test]
#[ignore = "Capture composer goal controls using an offscreen adapter"]
fn capture_thread_goal_composer() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let mut workbench = HeadlessWorkbench::new(state(dir.path()), [1280.0, 900.0]);
    workbench.run();
    let Some(frame) = gui::evidence::capture_or_skip(&mut workbench) else {
        return Ok(());
    };
    let output = std::env::var_os("EVORCH_GOAL_EVIDENCE")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp/opencode"));
    std::fs::create_dir_all(&output)?;
    let path = output.join("thread-goal-composer-1280x900.png");
    frame.save_png(&path)?;
    assert_eq!(image::image_dimensions(&path)?, (1280, 900));
    println!("{}", path.display());
    let mut paused = goal("A");
    paused.checks_paused = true;
    paused.review_enabled = true;
    paused.findings = vec!["The profiling observation needs a repeat measurement.".into()];
    paused.phase = ThreadGoalPhase::Repairing;
    workbench.state_mut().apply_events([event(paused)]);
    workbench.click_label("Show goal details");
    workbench.run();
    let frame = gui::evidence::capture_or_skip(&mut workbench).ok_or("adapter unavailable")?;
    let path = output.join("thread-goal-checks-paused-details-1280x900.png");
    frame.save_png(&path)?;
    assert_eq!(image::image_dimensions(&path)?, (1280, 900));
    println!("{}", path.display());
    Ok(())
}

#[test]
fn replay_skips_rows_of_removed_variants_and_restores_the_goal() {
    use gui::model::commands::{CommandSink, LoopEvent};
    use std::sync::{Arc, Mutex};
    #[derive(Default)]
    struct Observed {
        goals: Vec<String>,
    }
    struct Sink(Arc<Mutex<Observed>>);
    impl CommandSink for Sink {
        fn bind_thread_goal(&mut self, snapshot: &ThreadGoalSnapshot, _: &str) {
            self.0.lock().unwrap().goals.push(snapshot.goal_id.clone());
        }
        fn submit(&mut self, _: WorkbenchCommand) -> Vec<LoopEvent> {
            panic!("restoration cannot dispatch work")
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let config = storage::StorageConfig {
        db_path: dir.path().join("removed-variants.sqlite"),
        ..Default::default()
    };
    let storage = storage::Storage::open(config.clone()).unwrap();
    let mut snapshot = goal("A");
    snapshot.checks_paused = true;
    storage
        .handle()
        .append_event(None, &event(snapshot))
        .unwrap();
    // Rows of removed variants, in the format they were stored with. The retired
    // ThreadTodoUpdated row must neither fail replay nor restore any procedure.
    let connection = rusqlite::Connection::open(&config.db_path).unwrap();
    for (kind, payload) in [
        (
            "Snapshot",
            r#"{"kind":"Snapshot","payload":{"run_id":"run-1","call_id":"call-1","snapshot_id":"0123456789abcdef0123456789abcdef01234567","workspace_root":"/workspace"}}"#,
        ),
        (
            "Lifecycle",
            r#"{"kind":"Lifecycle","payload":{"kind":"WorkspaceWaitChanged","payload":{"run_id":"run-1","call_id":"call-1","waiting":null}}}"#,
        ),
        (
            "Orchestrator",
            r#"{"kind":"Orchestrator","payload":{"kind":"ThreadTodoUpdated","payload":{"snapshot":{"list_id":"list","thread_id":"A","revision":1,"items":[{"content":"old step","status":"in_progress"}]}}}}"#,
        ),
    ] {
        connection
            .execute(
                "INSERT INTO events \
                 (session_id, schema_version, monotonic_ns, wall_clock_ns, kind, payload) \
                 VALUES (NULL, ?1, 0, 0, ?2, ?3)",
                rusqlite::params![event_bus::SCHEMA_VERSION, kind, payload],
            )
            .unwrap();
    }
    drop(connection);
    storage.close();
    let observed = Arc::new(Mutex::new(Observed::default()));
    let mut state = bare_state(dir.path()).with_command_sink(Box::new(Sink(observed.clone())));
    state
        .restore_history(&storage::Database::open(&config).unwrap())
        .unwrap();
    let observed = observed.lock().unwrap();
    assert_eq!(observed.goals, ["goal-A"]);
    drop(observed);
    let mut workbench = HeadlessWorkbench::new(state, [1000.0, 800.0]);
    workbench.run();
    assert!(workbench.has_label("Resume goal checks"));
    assert!(!workbench.has_label("Show procedure details"));
}
