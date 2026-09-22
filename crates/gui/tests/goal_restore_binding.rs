use event_bus::{Event, OrchestratorEvent};
use gui::{
    app::WorkbenchState,
    fixture::DemoSource,
    model::commands::{CommandSink, LoopEvent, WorkbenchCommand},
};
use std::sync::{Arc, Mutex};
use workspace_ui::{ProjectId, SidebarState, ThreadId, UiSettings};

type Bindings = Arc<Mutex<Vec<(String, String, String)>>>;
struct CaptureSink(Bindings);
impl CommandSink for CaptureSink {
    fn bind_goal_context(&mut self, thread: &str, project: &str, run: &str) {
        self.0
            .lock()
            .unwrap()
            .push((thread.into(), project.into(), run.into()));
    }
    fn submit(&mut self, _: WorkbenchCommand) -> Vec<LoopEvent> {
        Vec::new()
    }
}
fn goal(project: &str, thread: &str, run: &str) -> Event {
    Event::new(OrchestratorEvent::GoalCreated {
        goal_id: format!("goal-{run}"),
        session_id: "gui".into(),
        project_id: project.into(),
        thread_id: thread.into(),
        root_run_id: run.into(),
        goal: "restore".into(),
        references: Vec::new(),
        constraints: Vec::new(),
        repo: "owner/repo".into(),
        base_ref: "main".into(),
    })
}
fn state(path: &std::path::Path, bindings: Bindings) -> WorkbenchState<DemoSource> {
    let mut sidebar = SidebarState::default();
    let project = ProjectId::new("current");
    sidebar
        .add_project(project.clone(), "current", path)
        .unwrap();
    sidebar.select_project(&project).unwrap();
    sidebar
        .create_thread(ThreadId::new("thread"), project, "thread")
        .unwrap();
    sidebar.switch_thread(&ThreadId::new("thread")).unwrap();
    WorkbenchState::new(DemoSource(Vec::new()), &UiSettings::default())
        .unwrap()
        .with_sidebar(sidebar)
        .with_command_sink(Box::new(CaptureSink(bindings)))
}
#[test]
fn live_and_replayed_goal_bind_only_to_current_project_and_existing_thread() {
    let dir = tempfile::tempdir().unwrap();
    let config = storage::StorageConfig {
        db_path: dir.path().join("events.sqlite3"),
        ..Default::default()
    };
    let storage = storage::Storage::open(config.clone()).unwrap();
    let events = vec![
        goal("previous", "thread", "run-1"),
        goal("current", "deleted", "run-2"),
        goal("current", "thread", "run-3"),
    ];
    for event in &events {
        storage.handle().append_event(Some("gui"), event).unwrap();
    }
    let live = Bindings::default();
    state(dir.path(), live.clone()).apply_events(events);
    let replay = Bindings::default();
    state(dir.path(), replay.clone())
        .restore_history(&storage::Database::open(&config).unwrap())
        .unwrap();
    let expected = vec![("thread".into(), "current".into(), "run-3".into())];
    assert_eq!(*live.lock().unwrap(), expected);
    assert_eq!(*replay.lock().unwrap(), expected);
}
