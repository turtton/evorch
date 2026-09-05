// allow: SIZE_OK - issue #65 T13 consolidates the three headless end-to-end
// scenarios (chained workbench flow, v1 migration, operator error paths) into
// this single new test file.
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use egui::{Key, Modifiers};
use egui_dock::SurfaceIndex;
use event_bus::{
    AgentMessage, AgentMessageEvent, AgentMessageKind, AgentRunPhase, CiState, DeliveryDisposition,
    Event, EventBus, GateSnapshot, LifecycleEvent, MergeBinding, MessageEvent, ProviderEvent,
    ToolEvent,
};
use gui::app::{ConversationFocus, WorkbenchError, WorkbenchState};
use gui::diff::{DiffMode, DiffState, FixtureDiffSource};
use gui::events::EventPump;
use gui::headless::HeadlessWorkbench;
use gui::model::commands::{
    CiStatus, GateItemView, GoalSubmission, LoopEvent, MergeApprovalView, MergeCommand,
    MergeDecision, PrRef, ReviewerStatus, WorkbenchCommand,
};
use gui::model::tasks::AgentRunSource;
use gui::model::transcript::TranscriptEntry;
use runtime::{AgentSummary, RunId};
use workspace_ui::{
    LayoutNode, PanelId, PanelKind, ProjectError, ProjectId, SidebarState, ThreadRunPhase,
    UiSettings, WORKSPACE_SCHEMA_VERSION,
};

const FIXTURE_DIFF_TEXT: &str = "fixture diff body (+12 -3)";

#[derive(Clone)]
struct MockSource(Vec<AgentSummary>);

impl MockSource {
    fn empty() -> Self {
        Self(Vec::new())
    }

    fn three_roles() -> Self {
        Self(vec![
            summary(1, "orchestrator", "orchestrator"),
            summary(2, "implementer", "worker"),
            summary(3, "reviewer", "reviewer"),
        ])
    }
}

impl AgentRunSource for MockSource {
    fn list(&self) -> Vec<AgentSummary> {
        self.0.clone()
    }
}

struct ChainedFixture {
    _runtime: tokio::runtime::Runtime,
    temp_dir: tempfile::TempDir,
    workspace_path: PathBuf,
    sidebar_path: PathBuf,
    bus: EventBus,
    repaint_rx: mpsc::Receiver<()>,
    workbench: HeadlessWorkbench<MockSource>,
}

impl ChainedFixture {
    fn new() -> Self {
        let runtime = tokio::runtime::Runtime::new().expect("test runtime");
        let bus = EventBus::new(64);
        let (repaint_tx, repaint_rx) = mpsc::channel();
        let pump = EventPump::spawn(
            runtime.handle(),
            bus.subscribe(),
            Some(Arc::new(move || {
                let _ = repaint_tx.send(());
            })),
        );
        let temp_dir = tempfile::tempdir().expect("temp dir");
        std::fs::create_dir(temp_dir.path().join("demo")).expect("project directory");
        let workspace_path = temp_dir.path().join("workspace.json");
        let sidebar_path = temp_dir.path().join("sidebar.json");
        let state = WorkbenchState::new(MockSource::three_roles(), &UiSettings::default())
            .expect("default state builds")
            .with_pump(pump)
            .with_save_path(&workspace_path)
            .with_sidebar_path(sidebar_path.clone())
            .with_diff_source(Arc::new(FixtureDiffSource::ready(FIXTURE_DIFF_TEXT)));
        let mut workbench = HeadlessWorkbench::new(state, [1200.0, 800.0]);
        workbench.run();
        Self {
            _runtime: runtime,
            temp_dir,
            workspace_path,
            sidebar_path,
            bus,
            repaint_rx,
            workbench,
        }
    }

    fn project_root(&self) -> PathBuf {
        self.temp_dir.path().join("demo")
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

fn pending_merge_view() -> MergeApprovalView {
    MergeApprovalView {
        pr: Some(PrRef {
            number: 65,
            title: "Workbench restructure".into(),
            url: "https://github.com/turtton/evorch/pull/65".into(),
        }),
        ci: CiStatus::Pending,
        reviewer: ReviewerStatus::Pending,
        diff_summary: Some("model-only change".into()),
        resolution: None,
        binding: Some(MergeBinding {
            token_id: "token-65".into(),
            repo: "turtton/evorch".into(),
            pr_number: 65,
            head_sha: "a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0".into(),
            snapshot: GateSnapshot {
                repo: "turtton/evorch".into(),
                pr_number: 65,
                base_ref: "main".into(),
                head_sha: "a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0".into(),
                ci: CiState::Green,
                criteria_round: 1,
                review_round: 1,
                reviewer_run_id: "run-review-1".into(),
            },
        }),
        gate: vec![
            GateItemView {
                label: "pull_request".into(),
                ok: true,
                detail: "#65 (turtton/evorch)".into(),
            },
            GateItemView {
                label: "ci".into(),
                ok: true,
                detail: "green".into(),
            },
        ],
        blocked: None,
    }
}

fn leaf_panels(node: &LayoutNode, leaves: &mut Vec<Vec<PanelId>>) {
    match node {
        LayoutNode::Split(split) => {
            leaf_panels(&split.first, leaves);
            leaf_panels(&split.second, leaves);
        }
        LayoutNode::Tabs(tabs) => leaves.push(tabs.panels.clone()),
    }
}

fn assert_default_v02_layout(workbench: &HeadlessWorkbench<MockSource>) {
    for id in [
        "sidebar-main",
        "agent-main",
        "agents-main",
        "diff-main",
        "terminal-main",
        "goal-main",
        "merge-main",
    ] {
        let tab = workbench
            .state()
            .dock()
            .find_tab(&PanelId::new(id))
            .expect("default v0.2 tab");
        assert_eq!(tab.surface, SurfaceIndex::main(), "{id} on main surface");
    }
    assert_eq!(
        workbench.state().dock().iter_all_tabs().count(),
        7,
        "no dynamic panes before the scenario opens them"
    );
}

fn activate_tab(workbench: &mut HeadlessWorkbench<MockSource>, panel_id: &str) {
    let path = workbench
        .state()
        .dock()
        .find_tab(&PanelId::new(panel_id))
        .expect("panel tab exists");
    workbench
        .state_mut()
        .dock_mut()
        .set_active_tab(path)
        .expect("tab can be activated");
    workbench.run();
}

fn wait_for_label(workbench: &mut HeadlessWorkbench<MockSource>, label: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !workbench.has_label(label) && Instant::now() < deadline {
        workbench.step();
        std::thread::yield_now();
    }
    assert!(workbench.has_label(label), "label {label:?} did not appear");
}

fn wait_for_diff_ready(workbench: &mut HeadlessWorkbench<MockSource>, expected: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let DiffState::Ready { text } = workbench.state().diff().state(&DiffMode::WorkingTree) {
            assert_eq!(text.as_str(), expected, "diff content mismatch");
            return;
        }
        assert!(
            Instant::now() < deadline,
            "working tree diff never became ready"
        );
        workbench.step();
        std::thread::yield_now();
    }
}

struct RunTranscriptExpectation<'a> {
    run_id: &'a str,
    tool_call_id: &'a str,
    agent_messages: &'a [&'a str],
}

fn assert_run_transcript(
    workbench: &HeadlessWorkbench<MockSource>,
    expected: RunTranscriptExpectation<'_>,
) {
    let entries = workbench
        .state()
        .transcripts()
        .run(expected.run_id)
        .unwrap_or_else(|| panic!("no transcript for {}", expected.run_id))
        .entries();
    let tool_calls = entries
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
    assert_eq!(
        tool_calls,
        vec![expected.tool_call_id],
        "run {} tool contamination",
        expected.run_id
    );
    assert_eq!(
        contents, expected.agent_messages,
        "run {} agent message contamination",
        expected.run_id
    );
}

fn dock_tab_ids(workbench: &HeadlessWorkbench<MockSource>) -> BTreeSet<String> {
    workbench
        .state()
        .dock()
        .iter_all_tabs()
        .map(|(_, tab)| tab.to_string())
        .collect()
}

fn sidebar_with_project(root: &std::path::Path) -> SidebarState {
    let mut sidebar = SidebarState::default();
    let project_id = ProjectId::new("demo");
    sidebar
        .add_project(project_id.clone(), "demo", root)
        .expect("project can be added");
    sidebar
        .select_project(&project_id)
        .expect("project can be selected");
    sidebar
}

#[test]
fn v02_end_to_end_chained_scenario() {
    // Given: a headless v0.2 workbench wired to an event bus, a fixture diff
    // source, the fixture loop adapter, and persistence paths.
    let mut fixture = ChainedFixture::new();

    // Then: the default layout is sidebar | conversation | workbench tabs.
    assert_default_v02_layout(&fixture.workbench);

    // When: a project and a thread are created through the public state API.
    let project_root = fixture.project_root();
    let project_id = fixture
        .workbench
        .state_mut()
        .add_project(project_root)
        .expect("project can be added");
    let thread_id = fixture
        .workbench
        .state_mut()
        .create_thread("integration thread")
        .expect("thread can be created");
    fixture.workbench.run();

    // Then: the thread becomes active under the selected project.
    let sidebar = fixture.workbench.state().sidebar();
    assert_eq!(sidebar.selected_project.as_ref(), Some(&project_id));
    assert_eq!(sidebar.active_thread.as_ref(), Some(&thread_id));

    // When: lifecycle, provider, tool, and agent-message events flow through the pump.
    for (id, name, role) in [
        (1, "orchestrator", "orchestrator"),
        (2, "implementer", "worker"),
        (3, "reviewer", "reviewer"),
    ] {
        fixture.emit(run_started(&format!("run-{id}"), name, role));
    }
    fixture.emit(Event::new(LifecycleEvent::AgentRunStateChanged {
        run_id: "run-2".into(),
        from: AgentRunPhase::Pending,
        to: AgentRunPhase::Running,
        reason: None,
    }));
    fixture.emit(request_started("run-2", "anthropic", "claude"));
    fixture.emit(request_completed("run-2", 120, 34));
    for run_id in ["run-1", "run-2", "run-3"] {
        fixture.emit(tool_started(
            run_id,
            &format!("tool-{run_id}"),
            &format!("call-{run_id}"),
        ));
    }
    fixture.emit(delivered("run-1", "run-2", "one-to-two"));
    fixture.emit(delivered("run-2", "run-3", "two-to-three"));
    fixture.emit(Event::new(MessageEvent::MessageDelta {
        delta: "thread-only progress".into(),
    }));

    // Then: every model received its events: thread attachment, run phase,
    // and the provider/tool telemetry row for run-2.
    assert_eq!(
        fixture.workbench.state().sidebar().threads[0].run_ids,
        vec!["run-1", "run-2", "run-3"]
    );
    assert_eq!(
        fixture.workbench.state().thread_phases().get("run-2"),
        Some(&ThreadRunPhase::Running)
    );
    for label in ["anthropic", "claude", "tool-run-2", "120 / 34"] {
        assert!(
            fixture.workbench.has_label(label),
            "missing agents telemetry label: {label}"
        );
    }

    // When: run-2 is drilled into from the agents table.
    fixture.workbench.click_label("run-2");
    fixture.workbench.step();
    fixture.workbench.run();

    // Then: the center pane focuses the run-2 transcript only.
    assert_eq!(
        fixture.workbench.state().focus(),
        &ConversationFocus::Agent("run-2".into())
    );
    assert!(fixture.workbench.has_label("run-2 / implementer / worker"));
    assert!(
        fixture
            .workbench
            .has_label("Tool tool-run-2 (call-run-2): Running")
    );
    assert!(
        !fixture
            .workbench
            .has_label("Tool tool-run-1 (call-run-1): Running")
    );

    // When: the operator returns to the thread conversation.
    fixture.workbench.click_label("← Thread");
    fixture.workbench.run();

    // Then: the center pane renders the thread transcript again.
    assert_eq!(
        fixture.workbench.state().focus(),
        &ConversationFocus::Thread
    );
    assert!(fixture.workbench.has_label("Message: thread-only progress"));

    // When: the default agent panes open for the three roles.
    fixture.workbench.click_label("Open default panes");
    fixture.workbench.run();

    // Then: three transcript tabs exist and each registry model holds only its
    // own tool call and directed agent messages.
    for run_id in ["run-1", "run-2", "run-3"] {
        assert!(
            fixture
                .workbench
                .state()
                .dock()
                .find_tab(&PanelId::new(format!("agent-{run_id}")))
                .is_some(),
            "missing transcript tab for {run_id}"
        );
    }
    assert_run_transcript(
        &fixture.workbench,
        RunTranscriptExpectation {
            run_id: "run-1",
            tool_call_id: "call-run-1",
            agent_messages: &["one-to-two"],
        },
    );
    assert_run_transcript(
        &fixture.workbench,
        RunTranscriptExpectation {
            run_id: "run-2",
            tool_call_id: "call-run-2",
            agent_messages: &["one-to-two", "two-to-three"],
        },
    );
    assert_run_transcript(
        &fixture.workbench,
        RunTranscriptExpectation {
            run_id: "run-3",
            tool_call_id: "call-run-3",
            agent_messages: &["two-to-three"],
        },
    );
    assert!(fixture.workbench.has_label("Transcript: run-3"));
    assert!(fixture.workbench.has_label("<- run-2: two-to-three"));
    assert!(!fixture.workbench.has_label("one-to-two"));

    // When: a working tree diff is requested and polled to completion.
    activate_tab(&mut fixture.workbench, "diff-main");
    fixture
        .workbench
        .state_mut()
        .request_diff(DiffMode::WorkingTree);
    wait_for_diff_ready(&mut fixture.workbench, FIXTURE_DIFF_TEXT);

    // Then: the fixture content renders in the diff pane.
    assert!(fixture.workbench.has_label(FIXTURE_DIFF_TEXT));

    // When: a goal is submitted through the state API.
    fixture.workbench.state_mut().goal_form_mut().goal = "consolidate the v0.2 workbench".into();
    fixture.workbench.state_mut().submit_goal();

    // Then: exactly one typed command is issued and the fixture loop adapter
    // answered with acceptance and a pending merge view.
    let submissions: Vec<&GoalSubmission> = fixture
        .workbench
        .state()
        .issued()
        .iter()
        .filter_map(|command| match command {
            WorkbenchCommand::SubmitGoal(submission) => Some(submission),
            WorkbenchCommand::DecideMerge(_)
            | WorkbenchCommand::PauseGoal { .. }
            | WorkbenchCommand::ResumeGoal { .. }
            | WorkbenchCommand::CancelGoal { .. } => None,
        })
        .collect();
    assert_eq!(submissions.len(), 1, "expected exactly one SubmitGoal");
    assert_eq!(submissions[0].project_id, "demo");
    assert_eq!(submissions[0].thread_id, "thread-1");
    assert_eq!(submissions[0].goal, "consolidate the v0.2 workbench");
    assert_eq!(
        fixture
            .workbench
            .state()
            .goal_form()
            .last_accepted
            .as_deref(),
        Some("goal-1")
    );
    assert!(fixture.workbench.state().merge().view.pr.is_some());

    // When: the loop publishes a pending merge view and the operator approves.
    fixture
        .workbench
        .state_mut()
        .apply_loop_event(LoopEvent::MergeStateUpdated(pending_merge_view()));
    fixture
        .workbench
        .state_mut()
        .decide_merge(MergeDecision::Approve);

    // Then: exactly one DecideMerge command carries the active thread.
    let decisions: Vec<&MergeCommand> = fixture
        .workbench
        .state()
        .issued()
        .iter()
        .filter_map(|command| match command {
            WorkbenchCommand::DecideMerge(merge) => Some(merge),
            WorkbenchCommand::SubmitGoal(_)
            | WorkbenchCommand::PauseGoal { .. }
            | WorkbenchCommand::ResumeGoal { .. }
            | WorkbenchCommand::CancelGoal { .. } => None,
        })
        .collect();
    assert_eq!(decisions.len(), 1, "expected exactly one DecideMerge");
    assert_eq!(decisions[0].thread_id, "thread-1");
    assert_eq!(decisions[0].decision, MergeDecision::Approve);
    assert_eq!(decisions[0].token_id.as_deref(), Some("token-65"));
    assert_eq!(
        fixture.workbench.state().merge().view.resolution,
        Some(MergeDecision::Approve)
    );

    // When: layout and sidebar are persisted and a fresh state is built from
    // the saved files.
    fixture.workbench.key_press(Modifiers::COMMAND, Key::S);
    fixture.workbench.run();
    fixture.workbench.state().save_sidebar();
    let saved_workspace =
        workspace_ui::load_workspace(&fixture.workspace_path).expect("saved workspace loads");
    let saved_sidebar =
        workspace_ui::load_sidebar(&fixture.sidebar_path).expect("saved sidebar loads");

    // Then: the saved tree keeps the three v0.2 regions plus the dynamic tabs.
    let mut leaves = Vec::new();
    leaf_panels(&saved_workspace.main.root, &mut leaves);
    let leaf_sets = leaves
        .iter()
        .map(|tabs| {
            tabs.iter()
                .map(|panel| panel.to_string())
                .collect::<BTreeSet<_>>()
        })
        .collect::<Vec<_>>();
    assert_eq!(
        leaf_sets,
        vec![
            BTreeSet::from(["sidebar-main".to_string()]),
            BTreeSet::from(["agent-main".to_string()]),
            BTreeSet::from([
                "agents-main".to_string(),
                "diff-main".to_string(),
                "terminal-main".to_string(),
                "goal-main".to_string(),
                "merge-main".to_string(),
                "agent-run-1".to_string(),
                "agent-run-2".to_string(),
                "agent-run-3".to_string(),
            ]),
        ],
        "saved tree must keep the v0.2 regions and dynamic transcript tabs"
    );
    for run_id in ["run-1", "run-2", "run-3"] {
        let panel = saved_workspace
            .panels
            .get(&PanelId::new(format!("agent-{run_id}")))
            .expect("dynamic transcript panel saved");
        assert_eq!(panel.kind, PanelKind::AgentTranscript);
        assert_eq!(panel.target.as_deref(), Some(run_id));
    }

    // When: the fresh state renders headlessly.
    let mut settings = UiSettings::default();
    settings.layout.workspace = Some(saved_workspace);
    let fresh = WorkbenchState::new(MockSource::empty(), &settings)
        .expect("fresh state builds")
        .with_sidebar(saved_sidebar);
    let mut fresh_bench = HeadlessWorkbench::new(fresh, [1200.0, 800.0]);
    fresh_bench.run();

    // Then: layout panels/tree and sidebar projects/threads match the original.
    assert_eq!(
        dock_tab_ids(&fresh_bench),
        dock_tab_ids(&fixture.workbench),
        "reloaded dock tabs must match the original workbench"
    );
    assert_eq!(
        fresh_bench.state().sidebar().projects,
        fixture.workbench.state().sidebar().projects
    );
    assert_eq!(
        fresh_bench.state().sidebar().threads,
        fixture.workbench.state().sidebar().threads
    );
    assert_eq!(
        fresh_bench
            .state()
            .sidebar()
            .active_thread
            .as_ref()
            .map(ToString::to_string),
        Some("thread-1".to_string())
    );
    assert_eq!(
        fresh_bench
            .state()
            .sidebar()
            .selected_project
            .as_ref()
            .map(ToString::to_string),
        Some("demo".to_string())
    );
}

#[test]
fn v1_settings_file_end_to_end_migration() {
    // Given: a persisted v0.1 workspace file whose panels carry no target.
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let settings_path = temp_dir.path().join("workspace.json");
    std::fs::write(
        &settings_path,
        include_str!("../../workspace-ui/tests/fixtures/workspace_v1.json"),
    )
    .expect("write v1 fixture");

    // When: it is loaded through the same persist API evorch-gui uses for --layout.
    let workspace = workspace_ui::load_workspace(&settings_path).expect("v1 workspace migrates");

    // Then: the schema is bumped to v2 and every legacy panel gained a
    // target:null default while validating cleanly.
    assert_eq!(workspace.version, WORKSPACE_SCHEMA_VERSION);
    for (id, kind) in [
        ("agent-main", PanelKind::Agent),
        ("tasks-main", PanelKind::Tasks),
        ("terminal-main", PanelKind::Terminal),
    ] {
        let panel = workspace
            .panels
            .get(&PanelId::new(id))
            .unwrap_or_else(|| panic!("legacy panel {id} registered"));
        assert_eq!(panel.kind, kind);
        assert_eq!(panel.target, None, "legacy panel {id} target default");
    }
    workspace.validate().expect("migrated workspace validates");

    // When: the migrated workspace renders in a headless workbench.
    let mut settings = UiSettings::default();
    settings.layout.workspace = Some(workspace);
    let state = WorkbenchState::new(MockSource::empty(), &settings)
        .expect("state builds from migrated workspace");
    let mut workbench = HeadlessWorkbench::new(state, [800.0, 600.0]);
    workbench.run();

    // Then: the legacy tabs render without panic. Tab titles are painted text,
    // so the body content proves each migrated pane actually rendered.
    for id in ["tasks-main", "agent-main", "terminal-main"] {
        assert!(
            workbench
                .state()
                .dock()
                .find_tab(&PanelId::new(id))
                .is_some(),
            "missing migrated tab {id}"
        );
    }
    assert!(workbench.has_label("Name"), "tasks table missing");
    assert!(workbench.has_label("Conversation"), "agent pane missing");
}

#[test]
fn operator_error_paths_stay_explicit() {
    // Given: a headless workbench with no projects.
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let state =
        WorkbenchState::new(MockSource::empty(), &UiSettings::default()).expect("state builds");
    let mut workbench = HeadlessWorkbench::new(state, [800.0, 600.0]);
    workbench.run();

    // When: a relative project path is added.
    let relative_error = workbench
        .state_mut()
        .add_project("v02-integration-missing/demo")
        .expect_err("relative path must be rejected");
    // And: a nonexistent absolute project path is added.
    let missing_error = workbench
        .state_mut()
        .add_project(temp_dir.path().join("missing-root"))
        .expect_err("missing root must be rejected");

    // Then: both rejections are typed errors, not panics, and the sidebar keeps
    // no phantom project.
    assert!(matches!(
        relative_error,
        WorkbenchError::Project(ProjectError::NotAbsolute)
    ));
    assert!(matches!(
        missing_error,
        WorkbenchError::Project(ProjectError::Canonicalize(_))
    ));
    workbench.run();
    assert!(workbench.state().sidebar().projects.is_empty());
    assert!(workbench.state().sidebar().selected_project.is_none());
    assert!(workbench.has_label("Projects"));

    // When: an active thread with a pending merge view rejects without a reason.
    let project_root = temp_dir.path().join("demo");
    std::fs::create_dir(&project_root).expect("project directory");
    {
        let state = workbench.state_mut();
        state.add_project(&project_root).expect("project added");
        state
            .create_thread("error path thread")
            .expect("thread created");
        state.apply_loop_event(LoopEvent::MergeStateUpdated(pending_merge_view()));
    }
    workbench.state_mut().decide_merge(MergeDecision::Reject {
        reason: String::new(),
    });
    workbench.run();

    // Then: no command is issued and the merge view stays unresolved.
    assert!(workbench.state().issued().is_empty());
    assert!(workbench.state().merge().view.resolution.is_none());

    // Given: a workbench whose diff source always fails.
    let error_state = WorkbenchState::new(MockSource::empty(), &UiSettings::default())
        .expect("state builds")
        .with_sidebar(sidebar_with_project(&project_root))
        .with_diff_source(Arc::new(FixtureDiffSource::error("boom")));
    let mut error_bench = HeadlessWorkbench::new(error_state, [800.0, 600.0]);
    activate_tab(&mut error_bench, "diff-main");

    // When: the working tree diff is requested from the pane.
    error_bench.click_label("Working tree");
    wait_for_label(&mut error_bench, "error: diff output I/O error: boom");

    // Then: the error is surfaced explicitly while the rest of the UI renders.
    assert!(matches!(
        error_bench.state().diff().state(&DiffMode::WorkingTree),
        DiffState::Error { .. }
    ));
    error_bench.step();
    assert!(error_bench.has_label("Projects"));
}
