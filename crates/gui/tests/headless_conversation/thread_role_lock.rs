use egui::{Key, Modifiers};
use event_bus::{Event, LifecycleEvent, OrchestratorEvent};
use gui::app::WorkbenchState;
use gui::fixture::DemoSource;
use gui::headless::HeadlessWorkbench;
use gui::model::commands::WorkbenchCommand;
use gui::model::composer::{ComposerRole, ProviderStatus};
use workspace_ui::{KeyAction, ProjectId, SidebarState, ThreadChatRole, ThreadId, UiSettings};

fn state(root: &std::path::Path, settings: &UiSettings) -> WorkbenchState<DemoSource> {
    let mut sidebar = SidebarState::default();
    let project = ProjectId::new("role-lock");
    sidebar.add_project(project.clone(), "test", root).unwrap();
    sidebar.select_project(&project).unwrap();
    for id in ["first", "second"] {
        sidebar
            .create_thread(ThreadId::new(id), project.clone(), id)
            .unwrap();
    }
    sidebar.switch_thread(&ThreadId::new("first")).unwrap();
    WorkbenchState::new(DemoSource(Vec::new()), settings)
        .unwrap()
        .with_sidebar(sidebar)
        .with_provider_status(ProviderStatus::Configured)
}

fn send(state: &mut WorkbenchState<DemoSource>, text: &str) {
    state.composer_mut().input = text.into();
    state.submit_composer();
}

#[test]
fn first_send_locks_role_button_remapped_shortcut_and_follow_up() {
    for role in [ComposerRole::Worker, ComposerRole::Orchestrator] {
        for remapped in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let mut settings = UiSettings::default();
            if remapped {
                settings
                    .keybinds
                    .bindings
                    .insert(KeyAction::CycleAgentRole, "Alt+R".parse().unwrap());
            }
            let mut state = state(dir.path(), &settings);
            state.composer_mut().role = role;
            assert!(!state.composer().role_locked);
            send(&mut state, "first message");
            assert!(state.composer().role_locked);
            assert_eq!(state.sidebar().threads[0].chat_role, Some(role.into()));
            let mut harness = HeadlessWorkbench::new(state, [1200.0, 900.0]);
            harness.run();
            harness.click_label("Message or /command");
            harness.run();
            harness.click_label(&format!("Role: {}", role.label()));
            harness.run();
            assert_eq!(harness.state().composer().role, role);
            if remapped {
                harness.key_press(Modifiers::ALT, Key::R);
                harness.run();
                assert_eq!(harness.state().composer().role, role);
            }
            send(harness.state_mut(), "follow up");
            assert!(matches!(harness.state().issued().last(),
                Some(WorkbenchCommand::SendChat(chat)) if chat.composer_role == role));
        }
    }
}

#[test]
fn thread_switch_and_restart_restore_lock_without_locking_unused_thread() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sidebar.json");
    let settings = UiSettings::default();
    let mut state = state(dir.path(), &settings).with_sidebar_path(path.clone());
    state.composer_mut().toggle_role();
    send(&mut state, "orchestrator task");
    state.switch_thread(ThreadId::new("second")).unwrap();
    assert!(!state.composer().role_locked);
    state.composer_mut().toggle_role();
    state.switch_thread(ThreadId::new("first")).unwrap();
    assert_eq!(state.composer().role, ComposerRole::Orchestrator);
    assert!(state.composer().role_locked);
    state.composer_mut().toggle_role();
    assert_eq!(state.composer().role, ComposerRole::Orchestrator);
    assert_eq!(state.sidebar().threads[1].chat_role, None);
    state.switch_thread(ThreadId::new("second")).unwrap();
    assert!(!state.composer().role_locked);
    send(&mut state, "worker task");

    let restored = workspace_ui::load_sidebar(&path).unwrap();
    let mut state = WorkbenchState::new(DemoSource(Vec::new()), &settings)
        .unwrap()
        .with_sidebar(restored);
    assert_eq!(state.composer().role, ComposerRole::Worker);
    assert!(state.composer().role_locked);
    state.composer_mut().toggle_role();
    assert_eq!(state.composer().role, ComposerRole::Worker);
    state.switch_thread(ThreadId::new("first")).unwrap();
    assert_eq!(state.composer().role, ComposerRole::Orchestrator);
    assert!(state.composer().role_locked);
}

#[test]
fn persisted_role_is_authoritative_for_submission_and_continue() {
    for role in [ComposerRole::Worker, ComposerRole::Orchestrator] {
        let dir = tempfile::tempdir().unwrap();
        let mut state = state(dir.path(), &UiSettings::default());
        state.composer_mut().role = role;
        send(&mut state, "initial");
        for text in ["follow up", "/continue"] {
            state.composer_mut().role_locked = false;
            state.composer_mut().toggle_role();
            send(&mut state, text);
            let actual = match state.issued().last().unwrap() {
                WorkbenchCommand::SendChat(chat) => chat.composer_role,
                WorkbenchCommand::ContinueChat(chat) => chat.composer_role,
                other => panic!("unexpected command: {other:?}"),
            };
            assert_eq!(actual, role);
            assert!(state.composer().role_locked);
            assert_eq!(state.sidebar().threads[0].chat_role, Some(role.into()));
        }
    }
}

#[test]
fn root_chat_events_lock_original_role_without_overwriting_it() {
    for role in [ThreadChatRole::Worker, ThreadChatRole::Orchestrator] {
        let dir = tempfile::tempdir().unwrap();
        let mut state = state(dir.path(), &UiSettings::default());
        let original = ComposerRole::from(role);
        for (index, name) in [original.label(), "worker", "orchestrator"]
            .iter()
            .enumerate()
        {
            let name = if *name == "worker" {
                "Worker"
            } else {
                "Orchestrator"
            };
            state.apply_events([Event::new(LifecycleEvent::AgentRunStarted {
                run_id: format!("run-{}", index + 1),
                parent_run_id: None,
                agent_name: format!("chat:{name}:first"),
                role: name.into(),
            })]);
            assert_eq!(state.composer().role, original);
            assert!(state.composer().role_locked);
            assert_eq!(state.sidebar().threads[0].chat_role, Some(role));
        }
    }
}

#[test]
fn goal_root_locks_only_its_thread_to_orchestrator() {
    let dir = tempfile::tempdir().unwrap();
    let mut state = state(dir.path(), &UiSettings::default());
    state.apply_events([Event::new(OrchestratorEvent::GoalCreated {
        goal_id: "goal-1".into(),
        session_id: "session".into(),
        project_id: "role-lock".into(),
        thread_id: "second".into(),
        goal: "coordinate work".into(),
        references: Vec::new(),
        constraints: Vec::new(),
        repo: "owner/repo".into(),
        base_ref: "main".into(),
        root_run_id: "run-1".into(),
    })]);
    assert!(!state.composer().role_locked);
    state.switch_thread(ThreadId::new("second")).unwrap();
    assert!(state.composer().role_locked);
    state.composer_mut().toggle_role();
    assert_eq!(state.composer().role, ComposerRole::Orchestrator);
    state.switch_thread(ThreadId::new("first")).unwrap();
    assert!(!state.composer().role_locked);
}

#[test]
fn escalation_thread_is_locked_to_orchestrator_without_chat_role_field() {
    let dir = tempfile::tempdir().unwrap();
    let mut sidebar = state(dir.path(), &UiSettings::default()).sidebar().clone();
    sidebar.threads[0].escalation_source_run_id = Some("run-1".into());
    let mut state = WorkbenchState::new(DemoSource(Vec::new()), &UiSettings::default())
        .unwrap()
        .with_sidebar(sidebar);
    assert!(state.composer().role_locked);
    assert_eq!(state.composer().role, ComposerRole::Orchestrator);
    state.composer_mut().toggle_role();
    assert_eq!(state.composer().role, ComposerRole::Orchestrator);
    state.switch_thread(ThreadId::new("second")).unwrap();
    assert!(!state.composer().role_locked);
    state.switch_thread(ThreadId::new("first")).unwrap();
    assert!(state.composer().role_locked);
    assert_eq!(state.composer().role, ComposerRole::Orchestrator);
}

#[test]
fn missing_provider_does_not_lock_unsent_thread() {
    let dir = tempfile::tempdir().unwrap();
    let mut state =
        state(dir.path(), &UiSettings::default()).with_provider_status(ProviderStatus::default());
    send(&mut state, "not sent");
    assert!(!state.composer().role_locked);
    assert_eq!(state.sidebar().threads[0].chat_role, None);
    state.composer_mut().toggle_role();
    assert_eq!(state.composer().role, ComposerRole::Orchestrator);
}
