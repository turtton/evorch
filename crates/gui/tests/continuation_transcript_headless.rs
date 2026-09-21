use event_bus::{AgentRunPhase, Event, LifecycleEvent, MessageEvent, OrchestratorEvent};
use gui::app::WorkbenchState;
use gui::headless::HeadlessWorkbench;
use gui::model::tasks::AgentRunSource;
use gui::model::transcript::TranscriptEntry;
use runtime::AgentSummary;
use workspace_ui::{PanelId, ProjectId, SidebarState, ThreadId, UiSettings};

struct Source;
impl AgentRunSource for Source {
    fn list(&self) -> Vec<AgentSummary> {
        Vec::new()
    }
}

fn started(run: &str, name: &str) -> Event {
    Event::new(LifecycleEvent::AgentRunStarted {
        run_id: run.into(),
        parent_run_id: Some("run-1".into()),
        agent_name: name.into(),
        role: "orchestrator".into(),
    })
}

fn delta(run: &str) -> Event {
    Event::new(MessageEvent::MessageDelta {
        run_id: Some(run.into()),
        delta: format!("reply from {run}"),
    })
}

fn recovered() -> HeadlessWorkbench<Source> {
    let temp = tempfile::tempdir().unwrap();
    let mut sidebar = SidebarState::default();
    let project = ProjectId::new("project");
    sidebar
        .add_project(project.clone(), "project", temp.path())
        .unwrap();
    sidebar.select_project(&project).unwrap();
    sidebar
        .create_thread(ThreadId::new("thread"), project, "conversation")
        .unwrap();
    sidebar.switch_thread(&ThreadId::new("thread")).unwrap();
    let mut state = WorkbenchState::new(Source, &UiSettings::default())
        .unwrap()
        .with_sidebar(sidebar);
    state.apply_events([
        started("run-1", "chat:thread"),
        Event::new(LifecycleEvent::AgentRunStateChanged {
            run_id: "run-1".into(),
            from: AgentRunPhase::Running,
            to: AgentRunPhase::Error,
            reason: Some("provider failure".into()),
        }),
    ]);
    let mut harness = HeadlessWorkbench::new(state, [1000.0, 700.0]);
    harness.run();
    harness
}

fn continuation() -> [Event; 3] {
    [
        Event::new(OrchestratorEvent::ContinuationDispatched {
            goal_id: "goal".into(),
            epoch: 1,
            trigger_run_id: "run-1".into(),
            new_run_id: "run-2".into(),
            unmet: Vec::new(),
        }),
        started("run-2", "goal/c1"),
        delta("run-2"),
    ]
}

#[test]
fn continuation_speaks_in_conversation_when_root_errors() {
    // Given: the conversation root has errored.
    let mut harness = recovered();
    // When: the supervisor dispatches and starts its continuation.
    harness.state_mut().apply_events(continuation());
    harness.run();
    // Then: the continuation's actual streamed text reaches the conversation.
    assert!(harness.state().transcripts().thread().entries().iter().any(
        |entry| matches!(entry, TranscriptEntry::Message { run_id: Some(run), text, .. }
            if run == "run-2" && text == "reply from run-2")
    ));
}

#[test]
fn continuation_has_no_subagent_pane_when_root_errors() {
    // Given: the conversation root has errored.
    let mut harness = recovered();
    // When: the supervisor dispatches and starts its continuation.
    harness.state_mut().apply_events(continuation());
    harness.run();
    // Then: the conversation partner does not open a subagent pane.
    assert!(
        harness
            .state()
            .dock()
            .find_tab(&PanelId::new("agent-run-2"))
            .is_none()
    );
}

#[test]
fn chat_replaces_continuation_as_conversation_root() {
    // Given: a recovered conversation.
    let mut harness = recovered();
    harness.state_mut().apply_events(continuation());
    // When: a subsequent chat starts and old runs emit late deltas.
    harness.state_mut().apply_events([
        started("run-3", "chat:thread"),
        delta("run-3"),
        delta("run-1"),
        delta("run-2"),
    ]);
    harness.run();
    // Then: only the current chat adds text to the conversation.
    let replies: Vec<_> = harness
        .state()
        .transcripts()
        .thread()
        .entries()
        .iter()
        .filter_map(|entry| match entry {
            TranscriptEntry::Message { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(replies, ["reply from run-2", "reply from run-3"]);
}
