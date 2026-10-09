use std::sync::{Arc, Mutex};

use event_bus::{AgentRunPhase, Event, LifecycleEvent, ToolEvent, UserQuestion};
use gui::app::{WorkbenchApp, WorkbenchState};
use gui::fixture::DemoSource;
use gui::model::system_notifications::{SystemNotification, SystemNotificationSink};
use workspace_ui::{ProjectId, SidebarState, ThreadId, UiSettings};

#[derive(Default)]
struct RecordingSink(Mutex<Vec<SystemNotification>>);

impl SystemNotificationSink for RecordingSink {
    fn send(&self, notification: SystemNotification) {
        self.0.lock().unwrap().push(notification);
    }
}

fn app(root: &std::path::Path, sink: Arc<RecordingSink>) -> WorkbenchApp<DemoSource> {
    let mut sidebar = SidebarState::default();
    let project = ProjectId::new("project");
    sidebar
        .add_project(project.clone(), "project", root)
        .unwrap();
    for id in ["one", "two"] {
        sidebar
            .create_thread(ThreadId::new(id), project.clone(), id)
            .unwrap();
    }
    sidebar.switch_thread(&ThreadId::new("one")).unwrap();
    WorkbenchApp(
        WorkbenchState::new(DemoSource(vec![]), &UiSettings::default())
            .unwrap()
            .with_sidebar(sidebar)
            .with_system_notifications(sink),
    )
}

// Drive the native eframe hook without ever rendering a UI. This is the
// execution path used by minimized/occluded windows.
fn logic(app: &mut WorkbenchApp<DemoSource>, focused: Option<bool>) {
    let ctx = egui::Context::default();
    let mut input = egui::RawInput {
        focused: focused.unwrap_or(true),
        ..Default::default()
    };
    input
        .viewports
        .get_mut(&egui::ViewportId::ROOT)
        .unwrap()
        .focused = focused;
    let _ = ctx.run_logic(&input, |ctx| {
        eframe::App::logic(app, ctx, &mut eframe::Frame::_new_kittest());
    });
}

fn started(run: &str, thread: &str, parent: Option<&str>) -> Event {
    Event::new(LifecycleEvent::AgentRunStarted {
        run_id: run.into(),
        parent_run_id: parent.map(str::to_owned),
        agent_name: if parent.is_some() {
            "child".into()
        } else {
            format!("chat:Worker:{thread}")
        },
        role: "worker".into(),
    })
}

fn turn(run: &str, context_len: u64) -> Event {
    Event::new(LifecycleEvent::TurnCompleted {
        run_id: run.into(),
        context_len,
    })
}

fn phase(run: &str, to: AgentRunPhase, reason: Option<&str>) -> Event {
    Event::new(LifecycleEvent::AgentRunStateChanged {
        run_id: run.into(),
        from: AgentRunPhase::Running,
        to,
        reason: reason.map(str::to_owned),
    })
}

fn question(id: &str, run: &str, root: &str, answer: Option<&str>) -> Event {
    Event::new(ToolEvent::UserQuestionUpdated {
        question: UserQuestion {
            id: id.into(),
            run_id: run.into(),
            root_run_id: root.into(),
            root_name: "chat:Worker:one".into(),
            recipient_run_ids: vec![],
            title: "Which output format?".into(),
            options: vec![],
            blocking: true,
            answer: answer.map(str::to_owned),
        },
    })
}

#[test]
fn inactive_logic_delivers_questions_and_subsequent_completion_once() {
    let dir = tempfile::tempdir().unwrap();
    let sink = Arc::new(RecordingSink::default());
    let mut app = app(dir.path(), sink.clone());
    logic(&mut app, Some(false));
    let asked = question("q", "root", "root", None);
    app.0.apply_events([
        started("root", "one", None),
        asked.clone(),
        asked,
        turn("root", 3),
    ]);
    logic(&mut app, Some(false));
    let notifications = sink.0.lock().unwrap();
    assert_eq!(notifications.len(), 1);
    assert!(notifications[0].body.contains("Which output format?"));
    drop(notifications);

    let completed = turn("root", 6);
    app.0.apply_events([
        question("q", "root", "root", Some("JSON")),
        phase("root", AgentRunPhase::Running, None),
        completed.clone(),
        completed,
        phase("root", AgentRunPhase::Done, None),
    ]);
    logic(&mut app, Some(false));
    assert_eq!(sink.0.lock().unwrap().len(), 2);
}

#[test]
fn focused_or_unknown_window_consumes_events_without_later_notifications() {
    for focused in [Some(true), None] {
        let dir = tempfile::tempdir().unwrap();
        let sink = Arc::new(RecordingSink::default());
        let mut app = app(dir.path(), sink.clone());
        logic(&mut app, focused);
        app.0.apply_events([
            started("root", "one", None),
            question("q", "root", "root", None),
        ]);
        logic(&mut app, focused);
        logic(&mut app, Some(false));
        app.0.apply_events([question("q", "root", "root", None)]);
        logic(&mut app, Some(false));
        assert!(sink.0.lock().unwrap().is_empty());
    }
}

#[test]
fn child_events_failed_runs_and_escalations_do_not_report_completed_work() {
    let dir = tempfile::tempdir().unwrap();
    let sink = Arc::new(RecordingSink::default());
    let mut app = app(dir.path(), sink.clone());
    logic(&mut app, Some(false));
    app.0.apply_events([
        started("root", "one", None),
        started("child", "one", Some("root")),
        question("child-q", "child", "root", None),
        question("answered", "root", "root", Some("JSON")),
        turn("child", 3),
        phase("child", AgentRunPhase::Done, None),
        phase("root", AgentRunPhase::Stopped, None),
        phase("root", AgentRunPhase::Error, Some("failed")),
        phase("root", AgentRunPhase::Done, Some("escalated")),
    ]);
    logic(&mut app, Some(false));
    assert!(sink.0.lock().unwrap().is_empty());
}

#[test]
fn another_threads_root_and_new_turns_each_deliver_completion() {
    let dir = tempfile::tempdir().unwrap();
    let sink = Arc::new(RecordingSink::default());
    let mut app = app(dir.path(), sink.clone());
    logic(&mut app, Some(false));
    app.0
        .apply_events([started("root", "two", None), turn("root", 3)]);
    logic(&mut app, Some(false));
    assert_eq!(sink.0.lock().unwrap().len(), 1);
    assert!(sink.0.lock().unwrap()[0].body.contains("two"));
    app.0
        .apply_events([phase("root", AgentRunPhase::Running, None), turn("root", 6)]);
    logic(&mut app, Some(false));
    assert_eq!(sink.0.lock().unwrap().len(), 2);
}

#[test]
fn successful_root_run_completion_is_delivered_once() {
    let dir = tempfile::tempdir().unwrap();
    let sink = Arc::new(RecordingSink::default());
    let mut app = app(dir.path(), sink.clone());
    logic(&mut app, Some(false));
    let done = phase("root", AgentRunPhase::Done, None);
    app.0
        .apply_events([started("root", "one", None), done.clone(), done]);
    logic(&mut app, Some(false));
    logic(&mut app, Some(false));
    assert_eq!(sink.0.lock().unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn minimized_logic_drains_the_live_validated_event_pump() {
    let dir = tempfile::tempdir().unwrap();
    let sink = Arc::new(RecordingSink::default());
    let mut app = app(dir.path(), sink.clone());
    app.0.apply_events([started("root", "one", None)]);
    let bus = Arc::new(event_bus::EventBus::new(64));
    let (repaint_tx, repaint_rx) = std::sync::mpsc::channel();
    let pump = gui::events::EventPump::spawn(
        &tokio::runtime::Handle::current(),
        bus.subscribe(),
        Some(Arc::new(move || {
            let _ = repaint_tx.send(());
        })),
    );
    app.0 = app.0.with_pump(pump);
    bus.emit(question("live", "root", "root", None));
    while sink.0.lock().unwrap().is_empty() {
        // Both subscription arrival and validation completion explicitly request
        // repaint. No rendering, timing assumptions, or polling delay is needed.
        repaint_rx.recv().unwrap();
        logic(&mut app, Some(false));
    }
    assert_eq!(sink.0.lock().unwrap().len(), 1);
}

#[test]
fn restoring_history_seeds_completed_turns_without_resending_them() {
    let dir = tempfile::tempdir().unwrap();
    let config = storage::StorageConfig {
        db_path: dir.path().join("history.sqlite3"),
        ..Default::default()
    };
    let storage = storage::Storage::open(config.clone()).unwrap();
    let mut bridge = gui::storage_bridge::StorageBridge::new(storage.handle(), "session");
    for event in [started("root", "one", None), turn("root", 3)] {
        bridge.handle_event(&event).unwrap();
    }
    storage.close();

    let sink = Arc::new(RecordingSink::default());
    let mut app = app(dir.path(), sink.clone());
    app.0
        .restore_history(&storage::Database::open(&config).unwrap())
        .unwrap();
    logic(&mut app, Some(false));
    // The runtime emits the same boundary again after restoring a waiting run.
    app.0
        .apply_events([phase("root", AgentRunPhase::Running, None), turn("root", 3)]);
    logic(&mut app, Some(false));
    assert!(sink.0.lock().unwrap().is_empty());
    app.0
        .apply_events([phase("root", AgentRunPhase::Running, None), turn("root", 6)]);
    logic(&mut app, Some(false));
    assert_eq!(sink.0.lock().unwrap().len(), 1);
}

#[test]
fn unfinished_goals_are_not_reported_complete() {
    for phase in [
        event_bus::ThreadGoalPhase::Working,
        event_bus::ThreadGoalPhase::Blocked,
        event_bus::ThreadGoalPhase::Complete,
    ] {
        let dir = tempfile::tempdir().unwrap();
        let sink = Arc::new(RecordingSink::default());
        let mut app = app(dir.path(), sink.clone());
        logic(&mut app, Some(false));
        app.0.apply_events([
            started("root", "one", None),
            Event::new(event_bus::OrchestratorEvent::ThreadGoalUpdated {
                snapshot: event_bus::ThreadGoalSnapshot {
                    goal_id: "goal".into(),
                    thread_id: "one".into(),
                    root_run_id: "root".into(),
                    related_root_run_ids: vec![],
                    objective: "Task".into(),
                    criteria: vec![],
                    checks: vec![],
                    phase,
                    review_enabled: false,
                    checks_paused: false,
                    work_stopped: false,
                    epoch: 1,
                    review_round: 0,
                    findings: vec![],
                    reason: None,
                    usage: Default::default(),
                    max_review_rounds: 1,
                    max_tokens: None,
                    original_request: "Task".into(),
                },
            }),
            turn("root", 3),
        ]);
        logic(&mut app, Some(false));
        assert_eq!(
            sink.0.lock().unwrap().len(),
            usize::from(phase == event_bus::ThreadGoalPhase::Complete)
        );
    }
}
