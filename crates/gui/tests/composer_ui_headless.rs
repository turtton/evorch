use gui::app::WorkbenchState;
use gui::fixture::DemoSource;
use gui::headless::HeadlessWorkbench;
use gui::model::commands::{ChatSubmission, WorkbenchCommand};
use gui::model::composer::{PROVIDER_MISSING_GUIDANCE, ProviderStatus};
use workspace_ui::{PanelId, ProjectId, SidebarState, ThreadId, UiSettings};

fn workbench(root: &std::path::Path, provider: ProviderStatus) -> HeadlessWorkbench<DemoSource> {
    let mut sidebar = SidebarState::default();
    let project_id = ProjectId::new("demo");
    sidebar
        .add_project(project_id.clone(), "demo", root)
        .expect("project added");
    sidebar
        .select_project(&project_id)
        .expect("project selected");
    sidebar
        .create_thread(ThreadId::new("thread-1"), project_id, "thread-1")
        .expect("thread created");
    sidebar
        .switch_thread(&ThreadId::new("thread-1"))
        .expect("thread selected");
    let state = WorkbenchState::new(DemoSource(Vec::new()), &UiSettings::default())
        .expect("default state builds")
        .with_sidebar(sidebar)
        .with_provider_status(provider);
    HeadlessWorkbench::new(state, [1200.0, 900.0])
}

#[test]
fn send_button_round_trip_issues_send_chat() {
    // Given: an active empty conversation with a configured provider.
    let temp = tempfile::tempdir().expect("temp dir");
    let mut harness = workbench(temp.path(), ProviderStatus::Configured);
    harness.state_mut().composer_mut().input = "hello agent".into();
    harness.run();
    // When: the conversation's Send button is clicked.
    harness.click_label("Send");
    harness.run();
    // Then: one chat is issued, rendered, and the composer remains available.
    assert_eq!(
        harness.state().issued(),
        &[WorkbenchCommand::SendChat(ChatSubmission {
            thread_id: "thread-1".into(),
            text: "hello agent".into(),
            model_preference: None,
        })]
    );
    assert!(harness.has_label("You: hello agent"));
    assert!(harness.state().composer().input.is_empty());
    assert!(harness.has_label("Send"));
}

#[test]
fn slash_completion_button_fills_input() {
    // Given: the slash completion menu in an active conversation.
    let temp = tempfile::tempdir().expect("temp dir");
    let mut harness = workbench(temp.path(), ProviderStatus::Configured);
    harness.state_mut().composer_mut().input = "/".into();
    harness.run();
    // When: the goal completion is selected.
    harness.click_label("/goal <text>");
    harness.run();
    // Then: arguments can be typed immediately after the trailing space.
    assert_eq!(harness.state().composer().input, "/goal ");
    assert!(harness.state().issued().is_empty());
}

#[test]
fn provider_guidance_visible_in_pane() {
    // Given: an active conversation without provider configuration.
    let temp = tempfile::tempdir().expect("temp dir");
    let mut harness = workbench(temp.path(), ProviderStatus::default());
    harness.state_mut().composer_mut().input = "/goal x".into();
    harness.run();
    assert!(harness.has_label(PROVIDER_MISSING_GUIDANCE));
    // When: a command is sent despite the missing chat provider.
    harness.click_label("Send");
    harness.run();
    // Then: the provider guard does not block goal submission.
    let [WorkbenchCommand::SubmitGoal(goal)] = harness.state().issued() else {
        panic!("expected exactly one SubmitGoal");
    };
    assert_eq!(goal.goal, "x");
}

#[test]
fn goal_command_submits_via_state_without_goal_pane() {
    // Given: the integrated workbench with a populated Goal form.
    let temp = tempfile::tempdir().expect("temp dir");
    let mut harness = workbench(temp.path(), ProviderStatus::Configured);
    harness.state_mut().goal_form_mut().goal = "implement issue #91".into();
    harness.run();
    // When: the retained state API submits the goal.
    harness.state_mut().submit_goal();
    harness.run();
    // Then: the existing submission and acceptance feedback still work.
    let [WorkbenchCommand::SubmitGoal(goal)] = harness.state().issued() else {
        panic!("expected exactly one SubmitGoal");
    };
    assert_eq!(goal.goal, "implement issue #91");
    assert!(
        harness
            .state()
            .dock()
            .find_tab(&PanelId::new("goal-main"))
            .is_none()
    );
    assert!(harness.has_label("accepted: goal-1"));
}

#[test]
fn composer_visible_on_empty_conversation_state() {
    // Given: an active thread with no transcript entries.
    let temp = tempfile::tempdir().expect("temp dir");
    let mut harness = workbench(temp.path(), ProviderStatus::Configured);
    // When: the conversation is rendered.
    harness.run();
    // Then: the empty state and composer coexist.
    assert!(harness.has_label("No messages yet"));
    assert!(harness.has_label("Send"));
}

#[test]
fn send_without_thread_shows_dispatch_notice() {
    // Given: no project or thread, so the conversation shows the thread prompt.
    let state = WorkbenchState::new(DemoSource(Vec::new()), &UiSettings::default())
        .expect("default state builds")
        .with_provider_status(ProviderStatus::Configured);
    let mut harness = HeadlessWorkbench::new(state, [1200.0, 900.0]);
    harness.state_mut().composer_mut().input = "hello".into();
    harness.run();
    // When: chat is sent from the real composer without a thread.
    harness.click_label("Send");
    harness.run();
    // Then: the pane routes to the dispatch guard instead of issuing a command.
    assert!(harness.has_label("Select or start a thread first"));
    assert!(harness.state().issued().is_empty());
}

#[test]
fn composer_stays_at_bottom_with_messages() {
    // Given
    let temp = tempfile::tempdir().expect("temp dir");
    let mut harness = workbench(temp.path(), ProviderStatus::Configured);
    harness.run();
    let empty_bottom = harness.label_rects("Send")[0].max.y;
    // When: submit 30 real messages to populate the conversation.
    for index in 0..30 {
        harness.state_mut().composer_mut().input = format!("message {index}");
        harness.state_mut().submit_composer();
    }
    harness.run();
    // Then
    let populated_bottom = harness.label_rects("Send")[0].max.y;
    assert!(
        (populated_bottom - empty_bottom).abs() <= 1.0,
        "empty={empty_bottom}, populated={populated_bottom}"
    );
}
