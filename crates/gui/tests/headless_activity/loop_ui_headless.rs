use gui::app::WorkbenchState;
use gui::headless::HeadlessWorkbench;
use gui::model::commands::{PacketReference, ReferenceKind, WorkbenchCommand};
use gui::model::tasks::AgentRunSource;
use runtime::AgentSummary;
use workspace_ui::{ProjectId, SidebarState, ThreadId, UiSettings};

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

fn workbench_with_thread(root: &std::path::Path) -> HeadlessWorkbench<MockSource> {
    let state = WorkbenchState::new(MockSource, &UiSettings::default())
        .expect("default state builds")
        .with_sidebar(sidebar_with_thread(root));
    HeadlessWorkbench::new(state, [800.0, 600.0])
}

#[test]
fn submit_goal_issues_typed_command_once_with_references_and_constraints() {
    // Given: an active project+thread and a goal form filled through the public state API
    let temp = tempfile::tempdir().expect("temp dir");
    let mut harness = workbench_with_thread(temp.path());
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
    harness.state_mut().submit_goal();
    harness.run();

    // Then: exactly one SubmitGoal command carries the form contents
    let submissions: Vec<_> = harness
        .state()
        .issued()
        .iter()
        .filter_map(|command| match command {
            WorkbenchCommand::SubmitGoal(submission) => Some(submission),
            WorkbenchCommand::SendChat(_)
            | WorkbenchCommand::ContinueChat(_)
            | WorkbenchCommand::CompactChat { .. }
            | WorkbenchCommand::StopChat { .. }
            | WorkbenchCommand::StopRun { .. }
            | WorkbenchCommand::DeliverFollowUpsNextTurn { .. }
            | WorkbenchCommand::CancelChat { .. }
            | WorkbenchCommand::SetGoalReview { .. }
            | WorkbenchCommand::SetGoalChecksPaused { .. }
            | WorkbenchCommand::DecideToolApproval { .. }
            | WorkbenchCommand::AnswerUserQuestion { .. }
            | WorkbenchCommand::SetWebToolsEnabled { .. } => None,
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
    harness.run();

    // Then: the pane explains why submission is unavailable
    assert!(harness.state().sidebar().active_thread.is_none());

    // When: the disabled Submit button is clicked anyway
    harness.state_mut().submit_goal();
    harness.run();

    // Then: no command is issued
    assert!(harness.state().issued().is_empty());
}
