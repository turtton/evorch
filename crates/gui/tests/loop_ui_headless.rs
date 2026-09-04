use gui::app::WorkbenchState;
use gui::headless::HeadlessWorkbench;
use gui::model::commands::{
    CiStatus, LoopEvent, MergeApprovalView, MergeDecision, PacketReference, PrRef, ReferenceKind,
    ReviewerStatus, WorkbenchCommand,
};
use gui::model::tasks::AgentRunSource;
use runtime::AgentSummary;
use workspace_ui::{PanelId, ProjectId, SidebarState, ThreadId, UiSettings};

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

fn workbench_with_thread(root: &std::path::Path) -> HeadlessWorkbench<MockSource> {
    let state = WorkbenchState::new(MockSource, &UiSettings::default())
        .expect("default state builds")
        .with_sidebar(sidebar_with_thread(root));
    HeadlessWorkbench::new(state, [800.0, 600.0])
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
        binding: None,
        gate: Vec::new(),
        blocked: None,
    }
}

fn issued_decisions(
    harness: &HeadlessWorkbench<MockSource>,
) -> Vec<&gui::model::commands::MergeCommand> {
    harness
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
        .collect()
}

#[test]
fn submit_goal_issues_typed_command_once_with_references_and_constraints() {
    // Given: an active project+thread and a goal form filled through the public state API
    let temp = tempfile::tempdir().expect("temp dir");
    let mut harness = workbench_with_thread(temp.path());
    activate_panel(&mut harness, "goal-main");
    harness.run();

    {
        let form = harness.state_mut().goal_form_mut();
        form.goal = "implement issue #65".into();
        form.references = vec![
            PacketReference {
                kind: ReferenceKind::Packet,
                value: "v02-gui-workbench-restructure".into(),
            },
            PacketReference {
                kind: ReferenceKind::Issue,
                value: "65".into(),
            },
        ];
        form.constraints = vec!["model only".into(), "no new deps".into()];
    }
    harness.run();

    // When: the Submit button is clicked
    harness.click_label("Submit");
    harness.run();

    // Then: exactly one SubmitGoal command carries the form contents
    let submissions: Vec<_> = harness
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
    let submission = submissions[0];
    assert_eq!(submission.project_id, "demo");
    assert_eq!(submission.thread_id, "thread-1");
    assert_eq!(submission.goal, "implement issue #65");
    assert_eq!(
        submission.references,
        vec![
            PacketReference {
                kind: ReferenceKind::Packet,
                value: "v02-gui-workbench-restructure".into(),
            },
            PacketReference {
                kind: ReferenceKind::Issue,
                value: "65".into(),
            },
        ]
    );
    assert_eq!(
        submission.constraints,
        vec!["model only".to_string(), "no new deps".to_string()]
    );
    // And: the fixture loop acceptance is shown in the pane
    assert!(harness.has_label("accepted: goal-1"));
}

#[test]
fn submit_disabled_without_active_thread() {
    // Given: a workbench without any project or thread
    let state =
        WorkbenchState::new(MockSource, &UiSettings::default()).expect("default state builds");
    let mut harness = HeadlessWorkbench::new(state, [800.0, 600.0]);
    activate_panel(&mut harness, "goal-main");
    harness.run();

    // Then: the pane explains why submission is unavailable
    assert!(harness.has_label("no active thread"));

    // When: the disabled Submit button is clicked anyway
    harness.click_label("Submit");
    harness.run();

    // Then: no command is issued
    assert!(harness.state().issued().is_empty());
}

#[test]
fn merge_view_updates_from_loop_event() {
    // Given: an active thread showing the merge pane
    let temp = tempfile::tempdir().expect("temp dir");
    let mut harness = workbench_with_thread(temp.path());
    activate_panel(&mut harness, "merge-main");
    harness.run();

    // When: the loop publishes a merge view for PR #65 with pending CI
    harness
        .state_mut()
        .apply_loop_event(LoopEvent::MergeStateUpdated(pending_merge_view()));
    harness.run();

    // Then: the PR info, badges, and diff summary are visible
    assert!(harness.has_label("PR #65"));
    assert!(harness.has_label("Workbench restructure"));
    assert!(harness.has_label("https://github.com/turtton/evorch/pull/65"));
    assert!(harness.has_label("ci: pending"));
    assert!(harness.has_label("reviewer: pending"));
    assert!(harness.has_label("model-only change"));
}

#[test]
fn approve_click_issues_exactly_one_command_even_if_clicked_twice() {
    // Given: a pending merge view on an active thread
    let temp = tempfile::tempdir().expect("temp dir");
    let mut harness = workbench_with_thread(temp.path());
    activate_panel(&mut harness, "merge-main");
    harness
        .state_mut()
        .apply_loop_event(LoopEvent::MergeStateUpdated(pending_merge_view()));
    harness.run();

    // When: Approve is clicked twice across two separate frames
    harness.click_label("Approve");
    harness.run();
    harness.click_label("Approve");
    harness.run();

    // Then: exactly one DecideMerge command was issued
    let decisions = issued_decisions(&harness);
    assert_eq!(decisions.len(), 1, "expected exactly one DecideMerge");
    assert_eq!(decisions[0].decision, MergeDecision::Approve);
    assert_eq!(decisions[0].thread_id, "thread-1");
    assert!(harness.has_label("resolved: approved"));
}

#[test]
fn reject_without_reason_is_blocked() {
    // Given: a pending merge view with an empty reject reason field
    let temp = tempfile::tempdir().expect("temp dir");
    let mut harness = workbench_with_thread(temp.path());
    activate_panel(&mut harness, "merge-main");
    harness
        .state_mut()
        .apply_loop_event(LoopEvent::MergeStateUpdated(pending_merge_view()));
    harness.run();

    // When: the disabled Reject button is clicked anyway
    harness.click_label("Reject");
    harness.run();

    // Then: nothing is issued and the view stays unresolved
    assert!(harness.state().issued().is_empty());
    assert!(harness.state().merge().view.resolution.is_none());
}
