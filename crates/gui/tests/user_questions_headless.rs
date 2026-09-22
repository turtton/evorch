use event_bus::{Event, LifecycleEvent, ToolEvent, UserQuestion};
use gui::{
    app::WorkbenchState, fixture::DemoSource, headless::HeadlessWorkbench,
    model::commands::WorkbenchCommand,
};
use workspace_ui::{ProjectId, SidebarState, ThreadId, UiSettings};
fn state(root: &std::path::Path) -> WorkbenchState<DemoSource> {
    let mut sidebar = SidebarState::default();
    let project = ProjectId::new("project");
    sidebar
        .add_project(project.clone(), "project", root)
        .unwrap();
    for id in ["one", "two"] {
        sidebar
            .create_thread(ThreadId::new(id), project.clone(), id)
            .unwrap();
    }
    sidebar.switch_thread(&ThreadId::new("one")).unwrap();
    WorkbenchState::new(DemoSource(vec![]), &UiSettings::default())
        .unwrap()
        .with_sidebar(sidebar)
}
fn started() -> Event {
    Event::new(LifecycleEvent::AgentRunStarted {
        run_id: "run-1".into(),
        parent_run_id: None,
        agent_name: "chat:Worker:one".into(),
        role: "worker".into(),
    })
}
fn question() -> UserQuestion {
    UserQuestion {
        id: "q-one".into(),
        run_id: "run-1".into(),
        root_run_id: "run-1".into(),
        root_name: "chat:Worker:one".into(),
        title: "Which output format?".into(),
        options: vec!["Markdown".into(), "JSON".into()],
        blocking: true,
        answer: None,
    }
}
#[test]
fn question_card_is_thread_scoped_and_selection_requires_explicit_send() {
    let dir = tempfile::tempdir().unwrap();
    let mut state = state(dir.path());
    state.apply_events([
        started(),
        Event::new(ToolEvent::UserQuestionUpdated {
            question: question(),
        }),
    ]);
    let mut ui = HeadlessWorkbench::new(state, [1100.0, 760.0]);
    ui.run();
    assert!(ui.has_label("Which output format?"));
    assert!(ui.state().issued().is_empty());
    ui.click_label("JSON");
    ui.run();
    assert!(ui.state().issued().is_empty());
    ui.click_label("回答を送信");
    ui.run();
    assert!(ui.state().issued().iter().any(|c|matches!(c,WorkbenchCommand::AnswerUserQuestion{thread_id,question_id,answer} if thread_id=="one" && question_id=="q-one" && answer=="JSON")));
    ui.state_mut().switch_thread(ThreadId::new("two")).unwrap();
    ui.run();
    assert!(!ui.has_label("Which output format?"));
    let mut answered = question();
    answered.answer = Some("custom text".into());
    ui.state_mut()
        .apply_events([Event::new(ToolEvent::UserQuestionUpdated {
            question: answered,
        })]);
    assert_eq!(ui.state().pending_user_questions().count(), 0);
}
#[test]
fn question_row_survives_restart_even_when_event_delivery_was_lost() {
    let dir = tempfile::tempdir().unwrap();
    let config = storage::StorageConfig {
        db_path: dir.path().join("questions.db"),
        ..Default::default()
    };
    let writer = storage::Storage::open(config.clone()).unwrap();
    writer
        .handle()
        .append_event(Some("gui"), &started())
        .unwrap();
    writer.handle().create_user_question(&question()).unwrap();
    let mut restored = state(dir.path());
    restored
        .restore_history(&storage::Database::open(&config).unwrap())
        .unwrap();
    let mut ui = HeadlessWorkbench::new(restored, [1100.0, 760.0]);
    ui.run();
    assert!(ui.has_label("Which output format?"));
    assert!(ui.has_label("JSON"));
}

#[test]
fn storage_acknowledgement_clears_card_without_a_source_run_event() {
    let dir = tempfile::tempdir().unwrap();
    let mut state = state(dir.path());
    state.apply_events([
        started(),
        Event::new(ToolEvent::UserQuestionUpdated {
            question: question(),
        }),
    ]);
    assert_eq!(state.pending_user_questions().count(), 1);
    state.apply_loop_event(gui::model::commands::LoopEvent::UserAnswerSaved {
        question_id: "q-one".into(),
    });
    assert_eq!(state.pending_user_questions().count(), 0);
}
