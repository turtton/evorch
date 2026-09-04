// allow: SIZE_OK - issue #65 requires all six T9 headless scenarios in this single new test file.
use std::sync::{Arc, mpsc};
use std::time::Duration;

use egui::{Key, Modifiers};
use egui_kittest::{Harness, kittest::Queryable};
use event_bus::{
    AgentMessage, AgentMessageEvent, AgentMessageKind, AgentRunPhase, DeliveryDisposition, Event,
    EventBus, MessageEvent, ProviderEvent, ToolEvent,
};
use gui::app::{ConversationFocus, WorkbenchState};
use gui::events::EventPump;
use gui::headless::HeadlessWorkbench;
use gui::model::tasks::AgentRunSource;
use gui::model::tasks::TasksModel;
use gui::model::telemetry::TelemetryOverlay;
use gui::model::transcript::TranscriptEntry;
use runtime::{AgentSummary, RunId};
use workspace_ui::{PanelId, PanelKind, UiSettings};

#[derive(Clone)]
struct MockSource(Vec<AgentSummary>);

impl AgentRunSource for MockSource {
    fn list(&self) -> Vec<AgentSummary> {
        self.0.clone()
    }
}

struct Fixture {
    _runtime: tokio::runtime::Runtime,
    _temp_dir: tempfile::TempDir,
    workspace_path: std::path::PathBuf,
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
        let workspace_path = temp_dir.path().join("workspace.json");
        let state = WorkbenchState::new(MockSource(runs), &UiSettings::default())
            .expect("default state builds")
            .with_pump(pump)
            .with_save_path(&workspace_path);
        let mut workbench = HeadlessWorkbench::new(state, [1200.0, 800.0]);
        workbench.run();
        Self {
            _runtime: runtime,
            _temp_dir: temp_dir,
            workspace_path,
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

#[test]
fn agents_rows_show_provider_tool_and_tokens_from_events() {
    // Given: one running agent and correlated provider, usage, and tool events.
    let mut fixture = Fixture::new(vec![summary(1, "worker-one", "worker")]);
    fixture.emit(request_started("run-1", "anthropic", "claude"));
    fixture.emit(request_completed("run-1", 120, 34));
    fixture.emit(tool_started("run-1", "read", "call-1"));

    // When: the Agents pane renders the latest telemetry row.
    fixture.workbench.run();

    // Then: every event-derived value is visible without substituting task metadata.
    for label in ["anthropic", "claude", "read", "120 / 34"] {
        assert!(fixture.workbench.has_label(label), "missing label: {label}");
    }
}

#[test]
fn missing_provider_renders_unknown_not_fabricated() {
    // Given: one run with task metadata but no provider telemetry.
    let mut tasks = TasksModel::new(MockSource(vec![summary(1, "worker-one", "worker")]));
    tasks.refresh();
    let telemetry = TelemetryOverlay::new();
    let mut harness = Harness::builder().build_ui_state(
        |ui, state: &mut (TasksModel<MockSource>, TelemetryOverlay)| {
            gui::panes::agents::agents_pane(ui, &state.0, &state.1);
        },
        (tasks, telemetry),
    );

    // When: the Agents pane renders the row.
    harness.run();

    // Then: absent provider, model telemetry, and current tool are explicitly unknown.
    assert_eq!(harness.query_all_by_label("unknown").count(), 3);
}

#[test]
fn clicking_agent_row_drills_center_into_its_transcript_and_back() {
    // Given: a thread transcript and two runs with distinct run-scoped events.
    let mut fixture = Fixture::new(vec![
        summary(1, "worker-one", "worker"),
        summary(2, "reviewer-two", "reviewer"),
    ]);
    fixture.emit(Event::new(MessageEvent::MessageDelta {
        delta: "thread-only text".into(),
    }));
    fixture.emit(tool_started("run-1", "read-one", "call-one"));
    fixture.emit(tool_started("run-2", "review-two", "call-two"));
    fixture.emit(delivered("run-2", "run-1", "run-two handoff"));

    // When: run-2 is selected from the Agents table.
    fixture.workbench.click_label("run-2");
    fixture.workbench.step();
    fixture.workbench.run();

    // Then: the center pane identifies run-2 and renders only its selected transcript.
    assert_eq!(
        fixture.workbench.state().focus(),
        &ConversationFocus::Agent("run-2".into())
    );
    assert!(
        fixture
            .workbench
            .has_label("run-2 / reviewer-two / reviewer")
    );
    assert!(
        fixture
            .workbench
            .has_label("Tool review-two (call-two): Running")
    );
    assert!(fixture.workbench.has_label("-> run-1: run-two handoff"));
    assert!(
        !fixture
            .workbench
            .has_label("Tool read-one (call-one): Running")
    );

    // When: the operator returns to the thread conversation.
    fixture.workbench.click_label("← Thread");
    fixture.workbench.run();

    // Then: the center pane returns to the thread transcript.
    assert_eq!(
        fixture.workbench.state().focus(),
        &ConversationFocus::Thread
    );
    assert!(fixture.workbench.has_label("Message: thread-only text"));
}

#[test]
fn default_panes_pick_orchestrator_latest_worker_reviewer_only_if_present() {
    // Given: orchestrator, two workers, and reviewer runs.
    let mut fixture = Fixture::new(vec![
        summary(1, "orchestrator", "orchestrator"),
        summary(2, "worker-old", "worker"),
        summary(3, "worker-latest", "worker"),
        summary(4, "reviewer", "reviewer"),
    ]);

    // When: default transcript panes are opened.
    fixture.workbench.click_label("Open default panes");
    fixture.workbench.run();

    // Then: the orchestrator, latest worker, and latest reviewer panes are present.
    assert_dynamic_panes(&mut fixture, &[1, 3, 4]);

    // Given: only orchestrator and worker roles are present.
    let mut fixture = Fixture::new(vec![
        summary(1, "orchestrator", "orchestrator"),
        summary(2, "worker", "worker"),
    ]);

    // When: default transcript panes are opened.
    fixture.workbench.click_label("Open default panes");
    fixture.workbench.run();

    // Then: exactly two real panes are created, with no reviewer placeholder.
    assert_dynamic_panes(&mut fixture, &[1, 2]);
    assert!(
        fixture
            .workbench
            .state()
            .dock()
            .find_tab(&PanelId::new("agent-run-3"))
            .is_none()
    );
}

#[test]
fn three_transcript_panes_do_not_mix_run_events() {
    // Given: three default roles receiving interleaved tool and agent-message events.
    let mut fixture = Fixture::new(vec![
        summary(1, "orchestrator", "orchestrator"),
        summary(2, "worker", "worker"),
        summary(3, "reviewer", "reviewer"),
    ]);
    for run_id in ["run-1", "run-2", "run-3"] {
        fixture.emit(tool_started(
            run_id,
            &format!("tool-{run_id}"),
            &format!("call-{run_id}"),
        ));
    }
    fixture.emit(delivered("run-1", "run-2", "one-to-two"));
    fixture.emit(delivered("run-2", "run-3", "two-to-three"));

    // When: all three default transcript panes are opened.
    fixture.workbench.click_label("Open default panes");
    fixture.workbench.run();

    // Then: each registry model contains only its own call and directed messages.
    assert_run_entries(
        &fixture.workbench,
        RunEntriesExpectation {
            run_id: "run-1",
            own_call_id: "call-run-1",
            messages: &["one-to-two"],
        },
    );
    assert_run_entries(
        &fixture.workbench,
        RunEntriesExpectation {
            run_id: "run-2",
            own_call_id: "call-run-2",
            messages: &["one-to-two", "two-to-three"],
        },
    );
    assert_run_entries(
        &fixture.workbench,
        RunEntriesExpectation {
            run_id: "run-3",
            own_call_id: "call-run-3",
            messages: &["two-to-three"],
        },
    );
    assert!(fixture.workbench.has_label("<- run-2: two-to-three"));
    assert!(!fixture.workbench.has_label("one-to-two"));
}

#[test]
fn close_and_reopen_pane_does_not_duplicate_entries() {
    // Given: one run pane with two routed transcript entries.
    let mut fixture = Fixture::new(vec![summary(1, "worker-one", "worker")]);
    fixture.workbench.click_label("Open pane");
    fixture.workbench.step();
    fixture.workbench.run();
    fixture.emit(tool_started("run-1", "read", "call-1"));
    fixture.emit(delivered("run-1", "run-2", "handoff once"));
    let before = fixture
        .workbench
        .state()
        .transcripts()
        .run("run-1")
        .expect("run transcript")
        .entries()
        .len();

    // When: the public dock API removes the tab and Open pane recreates it.
    let panel_id = PanelId::new("agent-run-1");
    let tab_path = fixture
        .workbench
        .state()
        .dock()
        .find_tab(&panel_id)
        .expect("agent tab");
    fixture
        .workbench
        .state_mut()
        .dock_mut()
        .remove_tab(tab_path);
    fixture.workbench.run();
    let agents_path = fixture
        .workbench
        .state()
        .dock()
        .find_tab(&PanelId::new("agents-main"))
        .expect("agents tab");
    fixture
        .workbench
        .state_mut()
        .dock_mut()
        .set_active_tab(agents_path)
        .expect("activate agents tab");
    fixture.workbench.run();
    fixture.workbench.click_label("Open pane");
    fixture.workbench.step();
    fixture.workbench.run();

    // Then: the pane is restored against the existing model without replay duplication.
    assert!(
        fixture
            .workbench
            .state()
            .dock()
            .find_tab(&PanelId::new("agent-run-1"))
            .is_some()
    );
    assert_eq!(
        fixture
            .workbench
            .state()
            .transcripts()
            .run("run-1")
            .expect("run transcript")
            .entries()
            .len(),
        before
    );
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

fn request_started(run_id: &str, provider: &str, model: &str) -> Event {
    Event::new(ProviderEvent::RequestStarted {
        request_id: format!("request-{run_id}"),
        provider: provider.into(),
        profile: None,
        protocol: "fixture".into(),
        model: model.into(),
        streaming: true,
        run_id: Some(run_id.into()),
    })
}

fn request_completed(run_id: &str, input_tokens: u64, output_tokens: u64) -> Event {
    Event::new(ProviderEvent::RequestCompleted {
        request_id: format!("request-{run_id}"),
        provider: "anthropic".into(),
        profile: None,
        protocol: "fixture".into(),
        model: "claude".into(),
        streaming: true,
        duration_ms: 10,
        input_tokens,
        output_tokens,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        finish_reason: "stop".into(),
        run_id: Some(run_id.into()),
    })
}

fn tool_started(run_id: &str, tool_name: &str, call_id: &str) -> Event {
    Event::new(ToolEvent::ToolStarted {
        tool_name: tool_name.into(),
        call_id: call_id.into(),
        run_id: Some(run_id.into()),
    })
}

fn delivered(sender: &str, recipient: &str, content: &str) -> Event {
    Event::new(AgentMessageEvent::Delivered {
        message: AgentMessage {
            message_id: format!("message-{sender}-{recipient}"),
            sender_run_id: sender.into(),
            recipient_run_id: recipient.into(),
            kind: AgentMessageKind::Send,
            content: content.into(),
            reply_to: None,
        },
        disposition: DeliveryDisposition::Aside,
    })
}

fn assert_dynamic_panes(fixture: &mut Fixture, expected: &[u64]) {
    for id in expected {
        let panel_id = PanelId::new(format!("agent-run-{id}"));
        assert!(
            fixture
                .workbench
                .state()
                .dock()
                .find_tab(&panel_id)
                .is_some()
        );
    }
    assert_eq!(
        fixture
            .workbench
            .state()
            .dock()
            .iter_all_tabs()
            .filter(|(_, tab)| tab.as_str().starts_with("agent-run-"))
            .count(),
        expected.len()
    );
    fixture.workbench.key_press(Modifiers::COMMAND, Key::S);
    fixture.workbench.run();
    let workspace = workspace_ui::load_from(&fixture.workspace_path).expect("saved workspace");
    for id in expected {
        let run_id = format!("run-{id}");
        let panel = workspace
            .panels
            .get(&PanelId::new(format!("agent-{run_id}")))
            .expect("agent transcript panel");
        assert_eq!(panel.kind, PanelKind::AgentTranscript);
        assert_eq!(panel.target.as_deref(), Some(run_id.as_str()));
    }
}

struct RunEntriesExpectation<'a> {
    run_id: &'a str,
    own_call_id: &'a str,
    messages: &'a [&'a str],
}

fn assert_run_entries(
    workbench: &HeadlessWorkbench<MockSource>,
    expected: RunEntriesExpectation<'_>,
) {
    let entries = workbench
        .state()
        .transcripts()
        .run(expected.run_id)
        .expect("run transcript")
        .entries();
    let calls = entries
        .iter()
        .filter_map(|entry| match entry {
            TranscriptEntry::Tool { call_id, .. } => Some(call_id.as_str()),
            TranscriptEntry::Message { .. }
            | TranscriptEntry::Reasoning { .. }
            | TranscriptEntry::AgentMessage { .. } => None,
        })
        .collect::<Vec<_>>();
    let contents = entries
        .iter()
        .filter_map(|entry| match entry {
            TranscriptEntry::AgentMessage { content, .. } => Some(content.as_str()),
            TranscriptEntry::Message { .. }
            | TranscriptEntry::Reasoning { .. }
            | TranscriptEntry::Tool { .. } => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(calls, vec![expected.own_call_id]);
    assert_eq!(contents, expected.messages);
}
