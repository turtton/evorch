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
fn todo(thread: &str, revision: u64) -> event_bus::ThreadTodoSnapshot {
    use event_bus::{ThreadTodoItem, ThreadTodoStatus::*};
    event_bus::ThreadTodoSnapshot {
        list_id: "procedures-A".into(),
        thread_id: thread.into(),
        revision,
        items: vec![
            ThreadTodoItem {
                content: "Collect observations".into(),
                status: Completed,
            },
            ThreadTodoItem {
                content: "Analyze the captured observations and determine the cause".into(),
                status: InProgress,
            },
            ThreadTodoItem {
                content: "Repeat measurements".into(),
                status: Pending,
            },
        ],
    }
}
fn todo_event(snapshot: event_bus::ThreadTodoSnapshot) -> Event {
    Event::new(OrchestratorEvent::ThreadTodoUpdated { snapshot })
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
fn shared_band_switches_one_readonly_detail_area_without_hiding_summaries() {
    let dir = tempfile::tempdir().unwrap();
    let mut workbench = HeadlessWorkbench::new(state(dir.path()), [1000.0, 800.0]);
    workbench
        .state_mut()
        .apply_events([todo_event(todo("A", 1))]);
    workbench.run();
    let procedures = "Procedures: Analyze the captured observations and determine the cause";
    let goal_label = format!("Goal: {}", goal("A").objective);
    let goal_rect = workbench.label_rects(&goal_label)[0];
    let procedure_rect = workbench.label_rects(procedures)[0];
    assert!(goal_rect.bottom() <= procedure_rect.top());
    assert!(procedure_rect.bottom() <= workbench.label_rects("Message or /command")[0].top());
    assert!(workbench.has_label("1/3"));
    workbench.state_mut().composer_mut().input = "Unsent procedure question".into();
    workbench.click_label("Show goal details");
    workbench.run();
    assert!(workbench.has_label("Profiling report: artifacts/profile.txt"));
    workbench.click_label("Show procedure details");
    workbench.run();
    assert!(!workbench.has_label("Profiling report: artifacts/profile.txt"));
    assert!(workbench.has_label("Collect observations"));
    assert!(workbench.has_label("Repeat measurements"));
    assert!(
        workbench.label_rects("Repeat measurements")[0].bottom()
            < workbench.label_rects("Message or /command")[0].top()
    );
    assert!(workbench.has_label("Pending"));
    assert!(workbench.has_label("Completed"));
    assert!(workbench.has_label(&goal_label));
    assert!(workbench.has_label(procedures));
    assert!(workbench.state().issued().is_empty());
    workbench.click_label("Show goal details");
    workbench.run();
    assert!(workbench.has_label("Profiling report: artifacts/profile.txt"));
    assert!(!workbench.has_label("Repeat measurements"));
    assert_eq!(
        workbench.state().composer().input,
        "Unsent procedure question"
    );
    workbench
        .state_mut()
        .switch_thread(ThreadId::new("B"))
        .unwrap();
    workbench.run();
    assert!(!workbench.has_label(procedures));
    assert!(!workbench.has_label("Hide goal details"));
}

#[test]
fn todo_only_completed_clear_and_stopped_states_do_not_dispatch_work() {
    use event_bus::ThreadTodoStatus;
    let dir = tempfile::tempdir().unwrap();
    let mut workbench = HeadlessWorkbench::new(bare_state(dir.path()), [1000.0, 800.0]);
    workbench.run();
    assert!(!workbench.has_label("Show procedure details"));
    assert!(!workbench.has_label("Show goal details"));
    let mut snapshot = todo("A", 1);
    snapshot.items[1].status = ThreadTodoStatus::Pending;
    workbench
        .state_mut()
        .apply_events([todo_event(snapshot.clone())]);
    workbench.run();
    assert!(workbench.has_label("Procedures pending"));
    assert!(!workbench.has_label("Pause goal checks"));
    // Bind an actual conversation run before stopping it.
    workbench.state_mut().apply_events([
        Event::new(event_bus::LifecycleEvent::AgentRunStarted {
            run_id: "run-1".into(),
            parent_run_id: None,
            agent_name: "chat:Worker:A".into(),
            role: "Worker".into(),
        }),
        Event::new(event_bus::LifecycleEvent::AgentRunStateChanged {
            run_id: "run-1".into(),
            from: event_bus::AgentRunPhase::Pending,
            to: event_bus::AgentRunPhase::Running,
            reason: None,
        }),
    ]);
    // Stopping a working run does not rewrite the stored procedure status.
    snapshot.revision = 2;
    snapshot.items[1].status = ThreadTodoStatus::InProgress;
    workbench.state_mut().apply_events([
        todo_event(snapshot.clone()),
        Event::new(event_bus::LifecycleEvent::AgentRunStateChanged {
            run_id: "run-1".into(),
            from: event_bus::AgentRunPhase::Running,
            to: event_bus::AgentRunPhase::Stopped,
            reason: Some("user stop".into()),
        }),
    ]);
    workbench.run();
    assert!(workbench.has_label("Procedures in progress (stored status)"));
    snapshot.revision = 3;
    for item in &mut snapshot.items {
        item.status = ThreadTodoStatus::Completed;
    }
    workbench
        .state_mut()
        .apply_events([todo_event(snapshot.clone())]);
    workbench.run();
    assert!(workbench.has_label("3/3"));
    assert!(workbench.has_label("Procedures: All procedures completed"));
    workbench.click_label("Show procedure details");
    workbench.run();
    snapshot.revision = 4;
    snapshot.items.clear();
    workbench
        .state_mut()
        .apply_events([todo_event(snapshot), todo_event(todo("A", 2))]);
    workbench.run();
    assert!(!workbench.has_label("Show procedure details"));
    workbench
        .state_mut()
        .apply_events([todo_event(todo("A", 5))]);
    workbench.run();
    assert!(workbench.has_label("Show procedure details"));
    assert!(!workbench.has_label("Hide procedure details"));
    assert!(workbench.state().issued().is_empty());
}

#[test]
fn procedure_replay_preserves_transfer_clear_and_current_sidebar_authority() {
    use gui::model::commands::{CommandSink, LoopEvent};
    use std::sync::{Arc, Mutex};
    #[derive(Default)]
    struct Observed {
        snapshots: Vec<event_bus::ThreadTodoSnapshot>,
        roots: usize,
    }
    struct Sink(Arc<Mutex<Observed>>);
    impl CommandSink for Sink {
        fn bind_thread_todo(&mut self, snapshot: &event_bus::ThreadTodoSnapshot) {
            self.0.lock().unwrap().snapshots.push(snapshot.clone());
        }
        fn bind_goal_context(&mut self, _: &str, _: &str, _: &str) {
            self.0.lock().unwrap().roots += 1;
        }
        fn submit(&mut self, _: WorkbenchCommand) -> Vec<LoopEvent> {
            panic!("restoration cannot dispatch work")
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let config = storage::StorageConfig {
        db_path: dir.path().join("procedures.sqlite"),
        ..Default::default()
    };
    let storage = storage::Storage::open(config.clone()).unwrap();
    let mut cleared = todo("B", 3);
    cleared.items.clear();
    let events = [
        todo_event(todo("A", 1)),
        todo_event(todo("B", 2)),
        todo_event(cleared),
        todo_event(todo("A", 1)),
        todo_event(event_bus::ThreadTodoSnapshot {
            list_id: "deleted-list".into(),
            ..todo("deleted", 1)
        }),
    ];
    for event in &events {
        storage.handle().append_event(None, event).unwrap();
    }
    storage.close();
    let mut live = HeadlessWorkbench::new(bare_state(dir.path()), [1000.0, 800.0]);
    live.state_mut()
        .apply_events([todo_event(todo("A", 1)), todo_event(todo("B", 2))]);
    live.run();
    assert!(!live.has_label("Show procedure details"));
    live.state_mut().switch_thread(ThreadId::new("B")).unwrap();
    live.run();
    assert!(live.has_label("Show procedure details"));
    assert!(live.has_label("1/3"));
    let observed = Arc::new(Mutex::new(Observed::default()));
    let mut state = bare_state(dir.path()).with_command_sink(Box::new(Sink(observed.clone())));
    state
        .restore_history(&storage::Database::open(&config).unwrap())
        .unwrap();
    let observed = observed.lock().unwrap();
    assert_eq!(observed.roots, 0);
    assert_eq!(observed.snapshots.last().unwrap().thread_id, "B");
    assert!(observed.snapshots.last().unwrap().items.is_empty());
    assert!(
        !observed
            .snapshots
            .iter()
            .any(|snapshot| snapshot.thread_id == "deleted")
    );
    let mut workbench = HeadlessWorkbench::new(state, [1000.0, 800.0]);
    workbench.run();
    assert!(!workbench.has_label("Show procedure details"));
    workbench
        .state_mut()
        .switch_thread(ThreadId::new("B"))
        .unwrap();
    workbench.run();
    assert!(!workbench.has_label("Show procedure details"));
}

#[test]
fn narrow_band_retains_controls_and_parallel_count() {
    use egui_kittest::{Harness, kittest::Queryable};
    let goal = goal("A");
    let mut snapshot = todo("A", 1);
    snapshot.items[2].status = event_bus::ThreadTodoStatus::InProgress;
    let model = gui::model::transcript::TranscriptModel::new();
    for width in [240.0, 329.0, 480.0] {
        let mut harness = Harness::builder()
            .with_size(egui::vec2(width, 600.0))
            .build_ui_state(
                |ui,
                 state: &mut (
                    gui::model::composer::ComposerModel,
                    gui::model::model_picker::ModelPickerState,
                )| {
                    gui::theme::install(ui.ctx());
                    gui::panes::agent::agent_pane(
                        ui,
                        &model,
                        None,
                        gui::panes::agent::ConversationContext {
                            goal: Some(&goal),
                            todo: Some(&snapshot),
                            requests: None,
                            task_rows: &[],
                            phase_unread: false,
                            has_project: true,
                            active_thread_title: Some("A"),
                            parent_thread: None,
                            child_threads: Vec::new(),
                            thread_metrics: None,
                            phase: None,
                            next_thread_title: String::new(),
                            model_picker: gui::panes::model_picker::ModelPickerContext {
                                profiles: &[],
                                preference: None,
                                default_model: None,
                                enabled: false,
                            },
                            sandbox_picker: Default::default(),
                            branch: None,
                        },
                        &mut state.0,
                        &mut state.1,
                    );
                },
                (
                    gui::model::composer::ComposerModel::default(),
                    gui::model::model_picker::ModelPickerState::default(),
                ),
            );
        harness.run_steps(16);
        for label in [
            "Show goal details",
            "Pause goal checks",
            "Enable independent review before completion",
            "Show procedure details",
            "1/3",
            "2 procedures in progress",
        ] {
            let rect = harness.get_by_label(label).rect();
            assert!(
                rect.left() >= 0.0 && rect.right() <= width,
                "{label} at {width}: {rect:?}"
            );
        }
        assert!(harness.query_by_label("Procedures: Analyze the captured observations and determine the cause · +1 in progress").is_some());
    }
}

#[test]
#[ignore = "Capture the combined composer band using an offscreen adapter"]
fn capture_goal_procedure_composer() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let output = std::env::var_os("EVORCH_TODO_EVIDENCE")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| "/tmp/evorch-todo-evidence".into());
    std::fs::create_dir_all(&output)?;
    for (name, preset) in [
        ("graphite", gui::theme::style::ThemePreset::Graphite),
        ("tokyo-night", gui::theme::style::ThemePreset::TokyoNight),
    ] {
        for width in [1280.0, 760.0] {
            let mut workbench = HeadlessWorkbench::new(state(dir.path()), [width, 900.0]);
            workbench
                .state_mut()
                .apply_events([todo_event(todo("A", 1))]);
            workbench.reload_theme(preset);
            workbench.run();
            let Some(frame) = gui::evidence::capture_or_skip(&mut workbench) else {
                return Ok(());
            };
            let path = output.join(format!("goal-procedures-{name}-{width:.0}.png"));
            frame.save_png(&path)?;
            println!("{}", path.display());
            workbench.click_label("Show procedure details");
            workbench.run();
            let frame =
                gui::evidence::capture_or_skip(&mut workbench).ok_or("adapter unavailable")?;
            let path = output.join(format!("goal-procedures-{name}-{width:.0}-details.png"));
            frame.save_png(&path)?;
            println!("{}", path.display());
        }
    }
    Ok(())
}
