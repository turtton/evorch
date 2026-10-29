use event_bus::{AgentRunPhase, Event, LifecycleEvent};
use gui::app::WorkbenchState;
use gui::headless::HeadlessWorkbench;
use gui::model::tasks::AgentRunSource;
use runtime::{AgentInspection, AgentSummary, RunId};
use workspace_ui::{ProjectId, SidebarState, ThreadId, ThreadRecord, UiSettings};

struct EmptySource;

impl AgentRunSource for EmptySource {
    fn list(&self) -> Vec<AgentSummary> {
        Vec::new()
    }

    fn inspect(&self, _: RunId) -> Option<AgentInspection> {
        None
    }
}

/// `(id, title, parent, pinned, created_at)`; newer threads were created later.
const THREADS: [(&str, &str, Option<&str>, bool, i64); 5] = [
    ("pin", "Pinned", None, true, 1),
    ("old", "OldFamily", None, false, 2),
    ("kid", "Kid", Some("old"), false, 3),
    ("mid", "MiddleIdle", None, false, 20),
    ("new", "NewestIdle", None, false, 30),
];

fn fixture(root: &std::path::Path) -> HeadlessWorkbench<EmptySource> {
    let mut sidebar = SidebarState::default();
    let project = ProjectId::new("project");
    sidebar
        .add_project(project.clone(), "Project", root)
        .unwrap();
    sidebar.select_project(&project).unwrap();
    for (id, title, parent, pinned, created_at) in THREADS {
        let mut thread = ThreadRecord::new(ThreadId::new(id), project.clone(), title);
        thread.parent_thread_id = parent.map(ThreadId::new);
        thread.pinned = pinned;
        thread.created_at = created_at;
        thread.run_ids = vec![format!("run-{id}")];
        sidebar.threads.push(thread);
    }
    sidebar.switch_thread(&ThreadId::new("new")).unwrap();
    let state = WorkbenchState::new(EmptySource, &UiSettings::default())
        .unwrap()
        .with_sidebar(sidebar)
        .with_sidebar_path(root.join("sidebar.json"));
    HeadlessWorkbench::new(state, [1000.0, 900.0])
}

fn start(gui: &mut HeadlessWorkbench<EmptySource>, thread: &str, phase: AgentRunPhase) {
    let run_id = format!("run-{thread}");
    gui.state_mut().apply_events([
        Event::new(LifecycleEvent::AgentRunStarted {
            run_id: run_id.clone(),
            parent_run_id: None,
            agent_name: format!("chat:Worker:{thread}"),
            role: "worker".into(),
        }),
        Event::new(LifecycleEvent::AgentRunStateChanged {
            run_id,
            from: AgentRunPhase::Pending,
            to: phase,
            reason: None,
        }),
    ]);
}

fn top(gui: &HeadlessWorkbench<EmptySource>, label: &str) -> f32 {
    gui.label_rects(label)[0].top()
}

#[test]
fn family_with_a_running_child_sits_directly_below_pinned_threads() {
    let temp = tempfile::tempdir().unwrap();
    let mut gui = fixture(temp.path());
    gui.run();
    // Given: an idle sidebar, newest first below the pinned thread.
    assert!(top(&gui, "Pinned") < top(&gui, "NewestIdle"));
    assert!(top(&gui, "NewestIdle") < top(&gui, "MiddleIdle"));
    assert!(top(&gui, "MiddleIdle") < top(&gui, "OldFamily"));

    // When: only the older family's child starts running.
    start(&mut gui, "kid", AgentRunPhase::Running);
    gui.run();

    // Then: the family moves up to just below the pinned thread, children stay under it.
    assert!(top(&gui, "Pinned") < top(&gui, "OldFamily"));
    assert!(top(&gui, "OldFamily") < top(&gui, "↳ Kid"));
    assert!(top(&gui, "↳ Kid") < top(&gui, "NewestIdle"));
    assert!(top(&gui, "NewestIdle") < top(&gui, "MiddleIdle"));
}
