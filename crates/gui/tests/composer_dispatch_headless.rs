use event_bus::{Event, MessageEvent};
use gui::app::WorkbenchState;
use gui::fixture::DemoSource;
use gui::headless::HeadlessWorkbench;
use gui::model::commands::{ChatSubmission, LoopEvent, WorkbenchCommand};
use gui::model::composer::{PROVIDER_MISSING_GUIDANCE, ProviderStatus};
use gui::model::transcript::TranscriptEntry;
use workspace_ui::{PanelId, ProjectId, SidebarState, ThreadId, UiSettings};

fn workbench(root: &std::path::Path, status: ProviderStatus) -> HeadlessWorkbench<DemoSource> {
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
        .with_provider_status(status);
    HeadlessWorkbench::new(state, [1200.0, 900.0])
}

fn submit(harness: &mut HeadlessWorkbench<DemoSource>, input: &str) {
    harness.state_mut().composer_mut().input = input.into();
    harness.state_mut().submit_composer();
    harness.run();
}

#[test]
fn image_only_send_button_carries_attachment() {
    let temp = tempfile::tempdir().expect("temp dir");
    let mut harness = workbench(temp.path(), ProviderStatus::Configured);
    let composer = harness.state_mut().composer_mut();
    composer.image_input_supported = true;
    assert!(composer.add_pasted_image("data:image/png;base64,aGVsbG8="));
    harness.run();
    harness.click_label("Send");
    harness.run();
    let [WorkbenchCommand::SendChat(chat)] = harness.state().issued() else {
        panic!("image-only chat must dispatch");
    };
    assert_eq!(chat.images[0].data, "aGVsbG8=");
    assert!(chat.text.is_empty());
}

#[test]
fn external_review_dispatches_from_gui() {
    let temp = tempfile::tempdir().expect("temp dir");
    let mut harness = workbench(temp.path(), ProviderStatus::Configured);
    let registry = &mut harness.state_mut().composer_mut().registry;
    registry.load_external("review\tReview files\t<path>");
    registry.executable = Some("printf".into());
    submit(&mut harness, "/review src");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    while harness.state().external_command_running() {
        assert!(std::time::Instant::now() < deadline);
        harness.step();
        std::thread::yield_now();
    }
    harness.run();
    assert!(harness.has_label("review"));
    assert!(harness.state().issued().is_empty());
    assert!(harness.state().composer().input.is_empty());
}

#[test]
#[cfg(unix)]
fn pending_discovery_does_not_block_gui_and_can_be_cancelled() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().expect("temp dir");
    let executable = temp.path().join("discovery");
    std::fs::write(&executable, "#!/bin/sh\nexec sleep 60\n").expect("script");
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700))
        .expect("permissions");
    let mut harness = workbench(temp.path(), ProviderStatus::Configured);
    harness.state_mut().load_external_commands(executable);
    assert!(harness.state().external_command_running());
    harness.step();
    harness.state_mut().cancel_external_command();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    while harness.state().external_command_running() {
        assert!(std::time::Instant::now() < deadline);
        harness.step();
        std::thread::yield_now();
    }
    assert!(harness.state().issued().is_empty());
}

#[test]
fn external_review_completion_and_help_are_available() {
    let temp = tempfile::tempdir().expect("temp dir");
    let mut harness = workbench(temp.path(), ProviderStatus::Configured);
    harness
        .state_mut()
        .composer_mut()
        .registry
        .load_external("review\tReview files\t<path>");
    harness.state_mut().composer_mut().input = "/rev".into();
    harness.run();
    harness.click_label("/review");
    harness.run();
    assert_eq!(harness.state().composer().input, "/review ");
    submit(&mut harness, "/help");
    assert!(harness.state().transcripts().thread().entries().iter().any(|entry|
        matches!(entry, TranscriptEntry::Notice { text } if text.lines().any(|line| line.starts_with("/review <path>")))));
}

#[test]
fn unsupported_image_preserves_draft_and_does_not_dispatch() {
    let temp = tempfile::tempdir().expect("temp dir");
    let mut harness = workbench(temp.path(), ProviderStatus::Configured);
    harness
        .state_mut()
        .composer_mut()
        .add_pasted_image("data:image/png;base64,aGVsbG8=");
    harness.run();
    harness.click_label("Send");
    harness.run();
    assert!(harness.state().issued().is_empty());
    assert_eq!(harness.state().composer().attachments.len(), 1);
    assert!(harness.state().composer().image_warning().is_some());
}

#[test]
fn cancel_chat_dispatches_active_thread_command() {
    let temp = tempfile::tempdir().expect("temp dir");
    let mut harness = workbench(temp.path(), ProviderStatus::Configured);
    submit(&mut harness, "hello");
    harness.state_mut().apply_events([
        Event::new(event_bus::LifecycleEvent::AgentRunStarted {
            run_id: "chat-1".into(),
            parent_run_id: None,
            agent_name: "chat:thread-1".into(),
            role: "Worker".into(),
        }),
        Event::new(event_bus::LifecycleEvent::AgentRunStateChanged {
            run_id: "chat-1".into(),
            from: event_bus::AgentRunPhase::Pending,
            to: event_bus::AgentRunPhase::Running,
            reason: None,
        }),
    ]);
    harness.step();
    harness.step();
    assert!(harness.has_label("Cancel"));
    harness.click_label("Cancel");
    harness.step();
    harness.step();
    assert_eq!(
        harness.state().issued().last(),
        Some(&WorkbenchCommand::CancelChat {
            thread_id: "thread-1".into(),
        })
    );
}

#[test]
fn send_chat_carries_thread_model_preference() {
    // Given
    let temp = tempfile::tempdir().unwrap();
    let mut harness = workbench(temp.path(), ProviderStatus::Configured);
    harness
        .state_mut()
        .set_thread_model_preference(Some(workspace_ui::ModelPreference {
            profile: "local".into(),
            model: Some("model-b".into()),
        }));
    // When
    submit(&mut harness, "selected model");
    // Then
    let [WorkbenchCommand::SendChat(chat)] = harness.state().issued() else {
        panic!("expected chat");
    };
    assert_eq!(
        chat.model_preference,
        Some(runtime::ModelPreference {
            profile: "local".into(),
            model: Some("model-b".into()),
        })
    );
}

#[test]
fn chat_send_issues_send_chat_and_shows_user_line() {
    // Given: an active thread with a configured provider.
    let temp = tempfile::tempdir().expect("temp dir");
    let mut harness = workbench(temp.path(), ProviderStatus::Configured);
    assert_eq!(
        harness.state().provider_status(),
        &ProviderStatus::Configured
    );
    // When: plain text is submitted.
    submit(&mut harness, "hello agent");
    // Then: the typed chat is issued and the user line is visible.
    assert_eq!(
        harness.state().issued(),
        &[WorkbenchCommand::SendChat(ChatSubmission {
            images: Vec::new(),
            thread_id: "thread-1".into(),
            text: "hello agent".into(),
            model_preference: None,
        })]
    );
    assert!(harness.has_label("You: hello agent"));
    assert!(harness.state().composer().input.is_empty());
}

#[test]
fn chat_send_renders_agent_reply() {
    // Given: a chat accepted by the fixture as chat-1.
    let temp = tempfile::tempdir().expect("temp dir");
    let mut harness = workbench(temp.path(), ProviderStatus::Configured);
    submit(&mut harness, "hello agent");
    // When: the attributed reply arrives through the event fold.
    harness
        .state_mut()
        .apply_events([Event::new(MessageEvent::MessageDelta {
            delta: "Hi there".into(),
            run_id: Some("chat-1".into()),
        })]);
    harness.run();
    // Then: the reply is visible alongside the user message.
    assert!(harness.has_label("Hi there"));
    assert!(harness.has_label("You: hello agent"));
    assert!(harness.state().transcripts().run("chat-1").is_some());
}

#[test]
fn goal_command_reuses_goal_flow() {
    // Given: an active thread without provider configuration.
    let temp = tempfile::tempdir().expect("temp dir");
    let mut harness = workbench(temp.path(), ProviderStatus::default());
    // When: a goal command is submitted.
    submit(&mut harness, "/goal implement issue #91");
    // Then: only the existing goal command path runs.
    let [WorkbenchCommand::SubmitGoal(goal)] = harness.state().issued() else {
        panic!("expected exactly one SubmitGoal");
    };
    assert_eq!(goal.project_id, "demo");
    assert_eq!(goal.thread_id, "thread-1");
    assert_eq!(goal.goal, "implement issue #91");
    assert_eq!(
        harness.state().goal_form().last_accepted.as_deref(),
        Some("goal-1")
    );
    assert!(harness.state().composer().input.is_empty());
}

#[test]
fn goal_command_without_args_shows_usage_without_focusing_a_tab() {
    // Given: an active thread.
    let temp = tempfile::tempdir().expect("temp dir");
    let mut harness = workbench(temp.path(), ProviderStatus::default());
    let path = harness
        .state()
        .dock()
        .find_tab(&PanelId::new("agents-main"))
        .expect("agents tab");
    let before = harness
        .state()
        .dock()
        .leaf(path.node_path())
        .expect("leaf")
        .active;
    // When: /goal has no arguments.
    submit(&mut harness, "/goal");
    // Then: usage is visible without changing tab focus or submitting.
    assert!(harness.has_label("usage: /goal <text>"));
    assert!(harness.state().issued().is_empty());
    assert_eq!(
        harness
            .state()
            .dock()
            .leaf(path.node_path())
            .expect("leaf")
            .active,
        before
    );
}

#[test]
fn help_command_lists_goal_and_help() {
    // Given: an active thread.
    let temp = tempfile::tempdir().expect("temp dir");
    let mut harness = workbench(temp.path(), ProviderStatus::default());
    // When: help is requested.
    submit(&mut harness, "/help");
    // Then: the command list is visible without issuing a command.
    assert!(harness.has_label(
        "/undo — Restore the previous workspace snapshot\n/redo — Restore the next workspace snapshot\n/goal <text> — Submit a goal to the orchestrator loop\n/help — Show available commands"
    ));
    assert!(harness.state().issued().is_empty());
}

#[test]
fn chat_without_provider_shows_guidance_and_issues_nothing() {
    // Given: the default, unconfigured provider status.
    let temp = tempfile::tempdir().expect("temp dir");
    let mut harness = workbench(temp.path(), ProviderStatus::default());
    // When: chat is submitted.
    submit(&mut harness, "hello");
    // Then: the dispatch records a guidance notice in the transcript (the pane
    // also renders the standing guidance line; UI visibility is covered by
    // composer_ui_headless), nothing is issued, and the draft is preserved.
    assert!(
        harness
            .state()
            .transcripts()
            .thread()
            .entries()
            .iter()
            .any(
                |entry| matches!(entry, TranscriptEntry::Notice { text } if text == PROVIDER_MISSING_GUIDANCE)
            )
    );
    assert!(harness.state().issued().is_empty());
    assert_eq!(harness.state().composer().input, "hello");
    // When: local commands are submitted after the blocked chat.
    submit(&mut harness, "/goal ok");
    // Then: goal submission remains available.
    assert!(
        matches!(harness.state().issued(), [WorkbenchCommand::SubmitGoal(goal)] if goal.goal == "ok")
    );
    // When: help is requested.
    submit(&mut harness, "/help");
    // Then: help is appended without an additional command.
    assert!(harness.has_label(
        "/undo — Restore the previous workspace snapshot\n/redo — Restore the next workspace snapshot\n/goal <text> — Submit a goal to the orchestrator loop\n/help — Show available commands"
    ));
    assert_eq!(harness.state().issued().len(), 1);
}

#[test]
fn unknown_command_shows_error_and_sends_nothing() {
    // Given: a configured thread.
    let temp = tempfile::tempdir().expect("temp dir");
    let mut harness = workbench(temp.path(), ProviderStatus::Configured);
    // When: an unknown slash command is submitted.
    submit(&mut harness, "/foo bar");
    // Then: an error is visible and the input is preserved.
    assert!(harness.has_label("unknown command /foo — type /help"));
    assert!(harness.state().issued().is_empty());
    assert_eq!(harness.state().composer().input, "/foo bar");
}

#[test]
fn empty_input_is_noop() {
    // Given: a configured thread, tested independently for each empty form.
    let temp = tempfile::tempdir().expect("temp dir");
    for input in ["", "  "] {
        let mut harness = workbench(temp.path(), ProviderStatus::Configured);
        let before = harness.state().transcripts().thread().entries().to_vec();
        // When: empty input is submitted.
        submit(&mut harness, input);
        // Then: no command, transcript entry, or input change occurs.
        assert!(harness.state().issued().is_empty());
        assert_eq!(harness.state().transcripts().thread().entries(), before);
        assert_eq!(harness.state().composer().input, input);
    }
}

#[test]
fn chat_without_thread_shows_notice() {
    // Given: no active project or thread, and the default missing provider.
    let state = WorkbenchState::new(DemoSource(Vec::new()), &UiSettings::default())
        .expect("default state builds");
    let mut harness = HeadlessWorkbench::new(state, [1200.0, 900.0]);
    // When: chat is submitted.
    submit(&mut harness, "hello");
    // Then: thread selection takes precedence over provider configuration.
    assert!(harness.has_label("Select or start a thread first"));
    assert!(harness.state().issued().is_empty());
    assert_eq!(harness.state().composer().input, "hello");
}

#[test]
fn chat_rejected_event_renders_notice() {
    // Given: a submitted chat (the fixture only accepts chats).
    let temp = tempfile::tempdir().expect("temp dir");
    let mut harness = workbench(temp.path(), ProviderStatus::Configured);
    submit(&mut harness, "hello");
    // When: a rejection is applied through the same loop-event API used by sinks.
    harness
        .state_mut()
        .apply_loop_event(LoopEvent::ChatRejected {
            thread_id: "thread-1".into(),
            reason: "provider unavailable".into(),
        });
    harness.run();
    // Then: the failure reason is visible in the conversation.
    assert!(harness.has_label("chat failed: provider unavailable"));
    assert_eq!(harness.state().issued().len(), 1);
}
