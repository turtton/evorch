use egui::{Key, Modifiers};
use gui::app::WorkbenchState;
use gui::fixture::DemoSource;
use gui::headless::HeadlessWorkbench;
use gui::keymap::Keymap;
use gui::model::commands::WorkbenchCommand;
use gui::model::composer::{ComposerModel, ComposerRole, ProviderStatus};
use workspace_ui::{KeyAction, KeybindSettings, ProjectId, SidebarState, ThreadId, UiSettings};

#[test]
fn tab_resolves_role_but_ctrl_tab_does_not() {
    // Given: the default bindings and forward/reverse/unrelated shortcuts.
    let keymap = Keymap::from_settings(&KeybindSettings::default());
    for (modifiers, expected) in [
        (Modifiers::NONE, Some(KeyAction::CycleAgentRole)),
        (Modifiers::SHIFT, Some(KeyAction::CycleAgentRole)),
        (Modifiers::CTRL, None),
        (Modifiers::COMMAND, None),
        (Modifiers::ALT, None),
    ] {
        let ctx = egui::Context::default();
        let raw = egui::RawInput {
            events: vec![
                egui::Event::ModifiersChanged(modifiers),
                egui::Event::Key {
                    key: Key::Tab,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers,
                },
            ],
            ..Default::default()
        };
        // When: resolve before any widgets process the input.
        let mut actual = None;
        let mut output = ctx.run_ui(raw, |ui| {
            actual = ui.input(|input| keymap.action_for_input(input))
        });
        output.textures_delta.clear();
        // Then: only plain Tab and Shift+Tab cycle the two roles.
        assert_eq!(actual, expected, "{modifiers:?}");
    }
}

#[test]
fn toggle_role_flips_worker_and_orchestrator() {
    // Given: a worker composer with a draft.
    let mut model = ComposerModel {
        input: "draft".into(),
        ..Default::default()
    };
    assert_eq!(model.role, ComposerRole::Worker);
    for expected in [ComposerRole::Orchestrator, ComposerRole::Worker] {
        // When: cycle once.
        model.toggle_role();
        // Then: the role flips without changing the draft.
        assert_eq!(model.role, expected);
        assert_eq!(model.input, "draft");
    }
}

#[test]
fn explicit_commands_keep_their_meaning_for_both_roles() {
    // Given: explicit commands override either selected role.
    for role in [ComposerRole::Worker, ComposerRole::Orchestrator] {
        let model = ComposerModel {
            role,
            ..Default::default()
        };
        for (raw, name) in [("/goal task", "goal"), ("/run task", "run")] {
            // When: parse the submission rather than the role default.
            let parsed = model.parse_submission(raw);
            // Then: the original command and arguments survive.
            assert!(
                matches!(parsed, gui::model::composer::ComposerInput::Command { spec, args }
                if spec.name == name && args == "task")
            );
        }
    }
}

#[test]
fn panel_override_can_rebind_role_cycling() {
    // Given: the user remaps the role shortcut away from Tab.
    let panel = config::PanelConfig {
        keybinds: [("cycle_agent_role".into(), "Alt+R".into())].into(),
        ..Default::default()
    };
    // When: resolve the panel configuration.
    let settings =
        gui::keymap::panel_keybinds(&KeybindSettings::default(), &panel).expect("override");
    // Then: the custom chord is retained.
    assert_eq!(
        settings.bindings[&KeyAction::CycleAgentRole].to_string(),
        "Alt+R"
    );
}

#[test]
#[ignore = "writes native offscreen role indicator evidence"]
fn capture_role_indicators() {
    // Given: both roles in the real desktop composer.
    let temp = tempfile::tempdir().expect("root");
    let mut harness = workbench(temp.path());
    let evidence = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/gui-evidence/composer-role");
    std::fs::create_dir_all(&evidence).expect("evidence directory");
    for role in [ComposerRole::Worker, ComposerRole::Orchestrator] {
        harness.state_mut().composer_mut().role = role;
        harness.run();
        // When: render through the native offscreen backend.
        let capture = harness.capture().expect("offscreen adapter");
        // Then: save a reviewable screenshot in this worktree's evidence directory.
        capture
            .save_png(&evidence.join(format!("{}.png", role.label())))
            .expect("PNG evidence");
    }
}

fn workbench(root: &std::path::Path) -> HeadlessWorkbench<DemoSource> {
    let mut sidebar = SidebarState::default();
    let project = ProjectId::new("role-test");
    sidebar
        .add_project(project.clone(), "role-test", root)
        .expect("project");
    sidebar.select_project(&project).expect("select");
    let thread = ThreadId::new("role-thread");
    sidebar
        .create_thread(thread.clone(), project, "thread")
        .expect("thread");
    sidebar.switch_thread(&thread).expect("switch");
    let state = WorkbenchState::new(DemoSource(Vec::new()), &UiSettings::default())
        .expect("state")
        .with_sidebar(sidebar)
        .with_provider_status(ProviderStatus::Configured);
    HeadlessWorkbench::new(state, [1200.0, 900.0])
}

#[test]
fn plain_submission_routes_by_target_role() {
    for role in [ComposerRole::Worker, ComposerRole::Orchestrator] {
        // Given: an active thread with plain text and a selected role.
        let temp = tempfile::tempdir().expect("root");
        let mut harness = workbench(temp.path());
        harness.state_mut().composer_mut().role = role;
        harness.state_mut().composer_mut().input = "ship feature".into();
        harness.run();
        // When: send through the real composer button.
        harness.click_label("Send");
        harness.run();
        // Then: only the corresponding command path is used.
        match role {
            ComposerRole::Worker => assert!(matches!(harness.state().issued(),
                [WorkbenchCommand::SendChat(chat)] if chat.text == "ship feature")),
            ComposerRole::Orchestrator => assert!(matches!(harness.state().issued(),
                [WorkbenchCommand::SubmitGoal(goal)] if goal.goal == "ship feature")),
        }
        assert!(harness.state().composer().input.is_empty());
    }
}

#[test]
fn tab_cycles_before_focused_composer_and_enter_still_sends() {
    for modifiers in [Modifiers::NONE, Modifiers::SHIFT] {
        // Given: text editing focus in the worker composer.
        let temp = tempfile::tempdir().expect("root");
        let mut harness = workbench(temp.path());
        harness.state_mut().composer_mut().input = "ship feature".into();
        harness.run();
        harness.click_label("Message or /command");
        harness.run();
        // When: cycle then send using the retained keyboard focus.
        harness.key_press(modifiers, Key::Tab);
        harness.run();
        assert_eq!(harness.state().composer().role, ComposerRole::Orchestrator);
        assert_eq!(harness.state().composer().input, "ship feature");
        harness.key_press(Modifiers::NONE, Key::Enter);
        harness.run();
        // Then: Tab neither inserts whitespace nor moves focus away from input.
        assert!(matches!(harness.state().issued(),
            [WorkbenchCommand::SubmitGoal(goal)] if goal.goal == "ship feature"));
    }
}
