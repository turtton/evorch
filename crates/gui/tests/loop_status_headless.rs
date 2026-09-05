// Headless tests for the goal loop status / merge binding views (issue #73 T1.5).
// OrchestratorEvents flow through the EventPump exactly like the production
// drain_pump path, and the panes must render the reducer's view state.

use std::sync::{Arc, mpsc};
use std::time::Duration;

use event_bus::{
    CiState, Event, EventBus, GateRejection, GateSnapshot, GoalStage, GoalState, MergeBinding,
    OrchestratorEvent,
};
use gui::app::WorkbenchState;
use gui::events::EventPump;
use gui::headless::HeadlessWorkbench;
use gui::model::commands::{LoopStatusView, MergeDecision, WorkbenchCommand};
use gui::model::tasks::AgentRunSource;
use runtime::AgentSummary;
use workspace_ui::{PanelId, ProjectId, SidebarState, ThreadId, UiSettings};

const HEAD_SHA: &str = "deadbee0deadbee0deadbee0deadbee0deadbee0";

struct MockSource;

impl AgentRunSource for MockSource {
    fn list(&self) -> Vec<AgentSummary> {
        Vec::new()
    }
}

fn sidebar_with_thread(root: &std::path::Path) -> SidebarState {
    let mut sidebar = SidebarState::default();
    let project_id = ProjectId::new("demo");
    sidebar
        .add_project(project_id.clone(), "demo", root)
        .expect("project can be added");
    sidebar
        .select_project(&project_id)
        .expect("project can be selected");
    sidebar
        .create_thread(ThreadId::new("thread-1"), project_id, "thread-1")
        .expect("thread can be created");
    sidebar
        .switch_thread(&ThreadId::new("thread-1"))
        .expect("thread can be selected");
    sidebar
}

fn activate_panel(harness: &mut HeadlessWorkbench<MockSource>, panel_id: &str) {
    let dock = harness.state_mut().dock_mut();
    let path = dock
        .find_tab(&PanelId::new(panel_id))
        .expect("panel tab exists");
    let leaf = dock.leaf_mut(path.node_path()).expect("leaf exists");
    leaf.set_active_tab(path.tab.0).expect("tab index is valid");
}

fn binding(token_id: &str) -> MergeBinding {
    MergeBinding {
        token_id: token_id.into(),
        repo: "turtton/evorch".into(),
        pr_number: 101,
        head_sha: HEAD_SHA.into(),
        snapshot: GateSnapshot {
            repo: "turtton/evorch".into(),
            pr_number: 101,
            base_ref: "main".into(),
            head_sha: HEAD_SHA.into(),
            ci: CiState::Green,
            criteria_round: 1,
            review_round: 1,
            reviewer_run_id: "run-review-1".into(),
        },
    }
}

fn goal_created(goal_id: &str) -> Event {
    Event::new(OrchestratorEvent::GoalCreated {
        goal_id: goal_id.into(),
        session_id: "session-1".into(),
        project_id: "evorch".into(),
        thread_id: "thread-1".into(),
        goal: "implement issue #73".into(),
        references: Vec::new(),
        constraints: Vec::new(),
        repo: "turtton/evorch".into(),
        base_ref: "main".into(),
        root_run_id: "run-root-1".into(),
    })
}

struct LoopFixture {
    _runtime: tokio::runtime::Runtime,
    _temp: tempfile::TempDir,
    bus: EventBus,
    repaint_rx: mpsc::Receiver<()>,
    workbench: HeadlessWorkbench<MockSource>,
}

impl LoopFixture {
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
        let temp = tempfile::tempdir().expect("temp dir");
        let state = WorkbenchState::new(MockSource, &UiSettings::default())
            .expect("default state builds")
            .with_pump(pump)
            .with_sidebar(sidebar_with_thread(temp.path()));
        let mut workbench = HeadlessWorkbench::new(state, [800.0, 600.0]);
        workbench.run();
        Self {
            _runtime: runtime,
            _temp: temp,
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

fn control_commands(harness: &HeadlessWorkbench<MockSource>) -> (Vec<&str>, Vec<&str>, Vec<&str>) {
    let mut pauses = Vec::new();
    let mut resumes = Vec::new();
    let mut cancels = Vec::new();
    for command in harness.state().issued() {
        match command {
            WorkbenchCommand::PauseGoal { goal_id } => pauses.push(goal_id.as_str()),
            WorkbenchCommand::ResumeGoal { goal_id } => resumes.push(goal_id.as_str()),
            WorkbenchCommand::CancelGoal { goal_id } => cancels.push(goal_id.as_str()),
            WorkbenchCommand::SubmitGoal(_) | WorkbenchCommand::DecideMerge(_) => {}
        }
    }
    (pauses, resumes, cancels)
}

#[test]
fn goal_pane_shows_state_stage_and_rejections_from_bus_events() {
    // Given: a headless workbench with the goal pane active
    let mut fixture = LoopFixture::new();
    activate_panel(&mut fixture.workbench, "goal-main");
    fixture.workbench.run();

    // When: orchestrator loop events flow through the event bus pump
    fixture.emit(goal_created("goal-1"));
    fixture.emit(Event::new(OrchestratorEvent::GoalStageChanged {
        goal_id: "goal-1".into(),
        from: GoalStage::Implementing,
        to: GoalStage::Reviewing,
    }));
    fixture.emit(Event::new(OrchestratorEvent::FinishRejected {
        goal_id: "goal-1".into(),
        run_id: "run-root-1".into(),
        rejections: vec![GateRejection::NoPullRequest],
    }));
    fixture.emit(Event::new(OrchestratorEvent::ReviewRoundStarted {
        goal_id: "goal-1".into(),
        round: 1,
        reviewer_run_id: "run-review-1".into(),
        head_sha: HEAD_SHA.into(),
    }));
    fixture.emit(Event::new(OrchestratorEvent::ContinuationDispatched {
        goal_id: "goal-1".into(),
        epoch: 1,
        trigger_run_id: "run-root-1".into(),
        new_run_id: "run-cont-1".into(),
        unmet: vec![GateRejection::NoPullRequest],
    }));

    // Then: the goal pane status block renders state, stage, rejections,
    // review round, and the continuation epoch
    for label in [
        "state: active",
        "stage: reviewing",
        "rejected: no_pull_request",
        "round: 1",
        "epoch: 1",
    ] {
        assert!(
            fixture.workbench.has_label(label),
            "missing goal status label: {label}"
        );
    }
    let status: &LoopStatusView = fixture.workbench.state().loop_status();
    assert_eq!(status.goal_id.as_deref(), Some("goal-1"));
    assert_eq!(status.state, Some(GoalState::Active));
    assert_eq!(status.stage, Some(GoalStage::Reviewing));
    assert_eq!(status.review_round, 1);
    assert_eq!(status.epoch, 1);
    assert_eq!(status.last_rejections, vec!["no_pull_request".to_string()]);
}

#[test]
fn merge_pane_approve_disabled_until_binding_and_shows_head_and_token() {
    // Given: a headless workbench with the merge pane active and no binding yet
    let mut fixture = LoopFixture::new();
    activate_panel(&mut fixture.workbench, "merge-main");
    fixture.workbench.run();

    // Then: no binding details are shown and the disabled Approve button
    // issues nothing
    assert!(!fixture.workbench.has_label("head: deadbee0"));
    fixture.workbench.click_label("Approve");
    fixture.workbench.run();
    assert!(
        fixture.workbench.state().issued().is_empty(),
        "unbound Approve must issue no command"
    );

    // When: the supervisor requests merge approval on the bus
    fixture.emit(Event::new(OrchestratorEvent::MergeApprovalRequested {
        goal_id: "goal-1".into(),
        binding: binding("token-1"),
    }));

    // Then: the binding head, token, and gate checklist rows are visible
    assert!(fixture.workbench.has_label("head: deadbee0"));
    assert!(fixture.workbench.has_label("token: token-1"));
    assert!(fixture.workbench.has_label("gate: pull_request ok"));
    assert!(fixture.workbench.has_label("gate: ci ok"));
    assert!(
        !fixture
            .workbench
            .has_label("blocked: review_rounds_exhausted")
    );

    // When: Approve is clicked
    fixture.workbench.click_label("Approve");
    fixture.workbench.run();

    // Then: exactly one DecideMerge command carries the binding token
    let decisions: Vec<_> = fixture
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
    assert_eq!(decisions[0].token_id.as_deref(), Some("token-1"));
    assert_eq!(decisions[0].decision, MergeDecision::Approve);
    assert_eq!(decisions[0].thread_id, "thread-1");
}

#[test]
fn pause_button_issues_pause_goal_command_once() {
    // Given: a headless workbench with the goal pane active and an active goal
    let mut fixture = LoopFixture::new();
    activate_panel(&mut fixture.workbench, "goal-main");
    fixture.emit(goal_created("goal-1"));

    // When: the Pause goal button is clicked once
    fixture.workbench.click_label("Pause goal");
    fixture.workbench.run();

    // Then: exactly one PauseGoal command carries the goal id, and no other
    // control command was issued
    let (pauses, resumes, cancels) = control_commands(&fixture.workbench);
    assert_eq!(pauses, vec!["goal-1"], "expected exactly one PauseGoal");
    assert!(resumes.is_empty());
    assert!(cancels.is_empty());

    // When: the goal pauses on the bus and Resume goal is clicked
    fixture.emit(Event::new(OrchestratorEvent::GoalStateChanged {
        goal_id: "goal-1".into(),
        from: GoalState::Active,
        to: GoalState::Paused,
        reason: "operator".into(),
    }));
    fixture.workbench.click_label("Resume goal");
    fixture.workbench.run();

    // Then: the paused state is shown and exactly one ResumeGoal is issued
    assert!(fixture.workbench.has_label("state: paused"));
    let (pauses, resumes, cancels) = control_commands(&fixture.workbench);
    assert_eq!(pauses.len(), 1);
    assert_eq!(resumes, vec!["goal-1"], "expected exactly one ResumeGoal");
    assert!(cancels.is_empty());
}
