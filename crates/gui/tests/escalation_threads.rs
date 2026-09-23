use std::sync::{Arc, Mutex};

use event_bus::{
    AgentRunPhase, EscalationMemoSummary, Event, LifecycleEvent, MessageEvent, ToolEvent,
    UserQuestion,
};
use gui::{
    app::WorkbenchState,
    fixture::DemoSource,
    headless::HeadlessWorkbench,
    model::{
        commands::{CommandSink, LoopEvent, WorkbenchCommand},
        transcript::TranscriptEntry,
    },
};
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

fn sidebar(path: &std::path::Path) -> SidebarState {
    let mut sidebar = SidebarState::default();
    let project = ProjectId::new("project");
    sidebar
        .add_project(project.clone(), "project", path)
        .unwrap();
    sidebar.select_project(&project).unwrap();
    for (id, title) in [("parent", "Worker task"), ("other", "Unrelated task")] {
        sidebar
            .create_thread(ThreadId::new(id), project.clone(), title)
            .unwrap();
    }
    sidebar.switch_thread(&ThreadId::new("other")).unwrap();
    sidebar
}

fn state(path: &std::path::Path, bindings: Bindings) -> WorkbenchState<DemoSource> {
    WorkbenchState::new(DemoSource(Vec::new()), &UiSettings::default())
        .unwrap()
        .with_sidebar(sidebar(path))
        .with_command_sink(Box::new(CaptureSink(bindings)))
}

fn started(run: &str, parent: Option<&str>, name: &str) -> Event {
    Event::new(LifecycleEvent::AgentRunStarted {
        run_id: run.into(),
        parent_run_id: parent.map(Into::into),
        agent_name: name.into(),
        role: "orchestrator".into(),
    })
}

fn escalation() -> Event {
    Event::new(LifecycleEvent::EscalationRequested {
        source_run_id: "run-1".into(),
        new_run_id: "run-2".into(),
        summary: EscalationMemoSummary {
            original_request: "Implement the feature".into(),
            escalation_reason: "Coordination required".into(),
            files_touched: Vec::new(),
            blockers: Vec::new(),
            suggested_next: "Inspect the design".into(),
        },
    })
}

fn message(run: &str, text: &str) -> Event {
    Event::new(MessageEvent::MessageDelta {
        run_id: Some(run.into()),
        delta: text.into(),
    })
}

fn events() -> Vec<Event> {
    vec![
        started("run-1", None, "chat:Worker:parent"),
        message("run-1", "worker output"),
        started("run-2", None, "escalation-orchestrator"),
        message("run-2", "early output"),
        escalation(),
        escalation(), // Re-delivery does not duplicate the child or initial notices.
        message("run-2", " and later output"),
        started("run-3", Some("run-2"), "child worker"),
        message("run-3", "private child output"),
        Event::new(LifecycleEvent::AgentRunStateChanged {
            run_id: "run-2".into(),
            from: AgentRunPhase::Running,
            to: AgentRunPhase::Done,
            reason: None,
        }),
    ]
}

fn assert_projection(state: &mut WorkbenchState<DemoSource>) {
    assert_eq!(state.sidebar().threads.len(), 3);
    let child = &state.sidebar().threads[2];
    assert_eq!(child.id, ThreadId::new("escalation-run-2"));
    assert_eq!(child.parent_thread_id, Some(ThreadId::new("parent")));
    assert_eq!(child.escalation_source_run_id.as_deref(), Some("run-1"));
    assert_eq!(child.run_ids, ["run-2", "run-3"]);
    assert_eq!(state.sidebar().threads[0].run_ids, ["run-1"]);
    assert!(state.transcript().entries().is_empty());
    state
        .switch_thread(ThreadId::new("escalation-run-2"))
        .unwrap();
    let entries = state.transcript().entries();
    assert!(entries.iter().any(|entry| matches!(entry,
        TranscriptEntry::Message { text, .. } if text == "early output and later output")));
    assert!(!entries.iter().any(|entry| matches!(entry,
        TranscriptEntry::Message { text, .. } if text.contains("private child"))));
    state.switch_thread(ThreadId::new("parent")).unwrap();
    assert_eq!(
        state
            .transcript()
            .entries()
            .iter()
            .filter(|entry| matches!(entry,
        TranscriptEntry::Notice { text } if text.starts_with("Orchestrator thread を開始")))
            .count(),
        1
    );
    assert!(
        state
            .transcript()
            .entries()
            .iter()
            .any(|entry| matches!(entry,
        TranscriptEntry::Notice { text } if text.contains("完了しました") && text.contains("結果: early output and later output")))
    );
}

#[test]
fn escalation_owns_a_child_conversation_and_links_both_directions() {
    let dir = tempfile::tempdir().unwrap();
    let bindings = Bindings::default();
    let mut state = state(dir.path(), bindings.clone());
    state.apply_events(events());
    assert_projection(&mut state);
    assert!(
        bindings
            .lock()
            .unwrap()
            .iter()
            .all(|(thread, project, root)| thread == "escalation-run-2"
                && project == "project"
                && root == "run-2")
    );
    assert!(
        state
            .dock()
            .find_tab(&workspace_ui::PanelId::new("agent-run-2"))
            .is_none()
    );
    let mut gui = HeadlessWorkbench::new(state, [1600.0, 1400.0]);
    gui.run();
    gui.click_label("↳ 子 thread: Orchestrator · Worker task");
    gui.run();
    assert_eq!(
        gui.state().sidebar().active_thread,
        Some(ThreadId::new("escalation-run-2"))
    );
    assert!(gui.has_label("early output and later output"));
    assert_eq!(
        gui.state().composer().role,
        gui::model::composer::ComposerRole::Orchestrator
    );
    gui.click_label("← 親 thread: Worker task");
    gui.run();
    assert_eq!(
        gui.state().sidebar().active_thread,
        Some(ThreadId::new("parent"))
    );
}

#[test]
fn persisted_escalation_replays_child_root_and_continuation_binding() {
    let dir = tempfile::tempdir().unwrap();
    let config = storage::StorageConfig {
        db_path: dir.path().join("events.db"),
        ..Default::default()
    };
    let storage = storage::Storage::open(config.clone()).unwrap();
    for event in events() {
        storage.handle().append_event(Some("gui"), &event).unwrap();
    }
    let mut live = state(dir.path(), Bindings::default());
    live.apply_events(events());
    let persisted = live.sidebar().clone();
    let bindings = Bindings::default();
    let mut replay = state(dir.path(), bindings.clone()).with_sidebar(persisted);
    replay
        .restore_history(&storage::Database::open(&config).unwrap())
        .unwrap();
    assert_projection(&mut replay);
    assert!(
        bindings
            .lock()
            .unwrap()
            .iter()
            .any(|(thread, _, run)| thread == "escalation-run-2" && run == "run-2")
    );
}

#[test]
fn subagent_questions_wait_for_orchestrator_and_only_root_questions_reach_user() {
    let dir = tempfile::tempdir().unwrap();
    let mut state = state(dir.path(), Bindings::default());
    state.apply_events(events());
    state
        .switch_thread(ThreadId::new("escalation-run-2"))
        .unwrap();
    for (run, title) in [
        ("run-3", "Subagent asks orchestrator"),
        ("run-2", "Orchestrator asks user"),
    ] {
        state.apply_events([Event::new(ToolEvent::UserQuestionUpdated {
            question: UserQuestion {
                id: run.into(),
                run_id: run.into(),
                root_run_id: "run-2".into(),
                root_name: "escalation-orchestrator".into(),
                title: title.into(),
                options: Vec::new(),
                blocking: true,
                answer: None,
            },
        })]);
    }
    let mut gui = HeadlessWorkbench::new(state, [1600.0, 1400.0]);
    gui.run();
    assert!(!gui.has_label("Subagent asks orchestrator"));
    assert!(gui.has_label("Orchestrator asks user"));
    gui.state_mut()
        .switch_thread(ThreadId::new("parent"))
        .unwrap();
    gui.state_mut()
        .apply_events([Event::new(ToolEvent::UserQuestionUpdated {
            question: UserQuestion {
                id: "direct-question".into(),
                run_id: "run-1".into(),
                root_run_id: "run-1".into(),
                root_name: "chat:Worker:parent".into(),
                title: "Direct worker asks user".into(),
                options: Vec::new(),
                blocking: true,
                answer: None,
            },
        })]);
    gui.run();
    assert!(gui.has_label("Direct worker asks user"));
    assert!(!gui.has_label("Orchestrator asks user"));
}
