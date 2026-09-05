use std::sync::{Arc, mpsc};
use std::time::Duration;

use event_bus::{AgentRunPhase, Event, EventBus, LifecycleEvent, MessageEvent};
use gui::app::WorkbenchState;
use gui::events::EventPump;
use gui::headless::HeadlessWorkbench;
use gui::model::tasks::AgentRunSource;
use gui::model::transcript::TranscriptEntry;
use runtime::{AgentSummary, RunId};
use workspace_ui::{ThreadRunPhase, UiSettings};

#[derive(Clone)]
struct MockSource(Vec<AgentSummary>);

impl AgentRunSource for MockSource {
    fn list(&self) -> Vec<AgentSummary> {
        self.0.clone()
    }
}

#[test]
fn apply_events_folds_lifecycle_and_message_into_thread_transcript() {
    // Given: a workbench state with a project and an active thread.
    let dir = tempfile::tempdir().expect("temp dir");
    let mut state = WorkbenchState::new(
        MockSource(vec![summary(1, "orchestrator", "orchestrator")]),
        &UiSettings::default(),
    )
    .expect("default state builds");
    state.add_project(dir.path()).expect("project added");
    state.create_thread("thread-1").expect("thread created");

    // When: lifecycle and message events are folded synchronously.
    state.apply_events(vec![
        run_started("run-1", "orchestrator", "orchestrator"),
        run_state_changed("run-1", AgentRunPhase::Pending, AgentRunPhase::Running),
        Event::new(MessageEvent::MessageDelta {
            delta: "thread-only text".into(),
        }),
    ]);

    // Then: phase, thread transcript, and sidebar attachment observe the fold.
    assert_eq!(
        state.thread_phases().get("run-1"),
        Some(&ThreadRunPhase::Running)
    );
    assert!(state.transcripts().thread().entries().iter().any(
        |entry| matches!(entry, TranscriptEntry::Message { text } if text == "thread-only text")
    ));
    assert_eq!(
        state.sidebar().threads[0].run_ids,
        vec!["run-1".to_string()]
    );
}

#[test]
fn apply_events_matches_pump_drain_ordering() {
    // Given: the same event vector fed through the EventPump and apply_events.
    let events = vec![
        run_started("run-1", "worker-one", "worker"),
        run_state_changed("run-1", AgentRunPhase::Pending, AgentRunPhase::Running),
        Event::new(MessageEvent::MessageDelta {
            delta: "thread-only text".into(),
        }),
    ];

    let mut fixture = Fixture::new(vec![summary(1, "worker-one", "worker")]);
    for event in &events {
        fixture.emit(event.clone());
    }
    let pump_entries = fixture
        .workbench
        .state()
        .transcripts()
        .thread()
        .entries()
        .to_vec();
    let pump_phases = fixture.workbench.state().thread_phases().clone();

    let mut sync_state = WorkbenchState::new(
        MockSource(vec![summary(1, "worker-one", "worker")]),
        &UiSettings::default(),
    )
    .expect("default state builds");
    sync_state.apply_events(events);

    // Then: both paths fold identical thread transcript entries and phases.
    assert_eq!(sync_state.transcripts().thread().entries(), pump_entries);
    assert_eq!(sync_state.thread_phases(), &pump_phases);
}

struct Fixture {
    _runtime: tokio::runtime::Runtime,
    _temp_dir: tempfile::TempDir,
    bus: EventBus,
    repaint_rx: mpsc::Receiver<()>,
    workbench: HeadlessWorkbench<MockSource>,
}

impl Fixture {
    fn new(runs: Vec<AgentSummary>) -> Self {
        let runtime = tokio::runtime::Runtime::new().expect("test runtime");
        let bus = EventBus::new(32);
        let (repaint_tx, repaint_rx) = mpsc::channel();
        let pump = EventPump::spawn(
            runtime.handle(),
            bus.subscribe(),
            Some(Arc::new(move || {
                let _ = repaint_tx.send(());
            })),
        );
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let state = WorkbenchState::new(MockSource(runs), &UiSettings::default())
            .expect("default state builds")
            .with_pump(pump);
        let mut workbench = HeadlessWorkbench::new(state, [1200.0, 800.0]);
        workbench.run();
        Self {
            _runtime: runtime,
            _temp_dir: temp_dir,
            bus,
            repaint_rx,
            workbench,
        }
    }

    fn emit(&mut self, event: Event) {
        self.bus.emit(event);
        self.repaint_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("event repaint");
        self.workbench.run();
    }
}

fn summary(id: u64, name: &str, role: &str) -> AgentSummary {
    AgentSummary {
        run_id: RunId::new(id),
        name: name.into(),
        role_name: role.into(),
        phase: AgentRunPhase::Running,
        model: format!("task-model-{id}"),
    }
}

fn run_started(run_id: &str, agent_name: &str, role: &str) -> Event {
    Event::new(LifecycleEvent::AgentRunStarted {
        run_id: run_id.into(),
        parent_run_id: None,
        agent_name: agent_name.into(),
        role: role.into(),
    })
}

fn run_state_changed(run_id: &str, from: AgentRunPhase, to: AgentRunPhase) -> Event {
    Event::new(LifecycleEvent::AgentRunStateChanged {
        run_id: run_id.into(),
        from,
        to,
        reason: None,
    })
}
