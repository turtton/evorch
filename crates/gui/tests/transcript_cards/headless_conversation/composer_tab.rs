use egui::{Event, Key, Modifiers};
use gui::app::WorkbenchState;
use gui::fixture::DemoSource;
use gui::headless::HeadlessWorkbench;
use gui::model::composer::ComposerRole;
use workspace_ui::{ProjectId, SidebarState, ThreadId, UiSettings};

fn workbench(root: &std::path::Path) -> HeadlessWorkbench<DemoSource> {
    let mut sidebar = SidebarState::default();
    let project = ProjectId::new("tab-test");
    sidebar
        .add_project(project.clone(), "tab-test", root)
        .expect("project");
    sidebar.select_project(&project).expect("select");
    let thread = ThreadId::new("tab-thread");
    sidebar
        .create_thread(thread.clone(), project, "thread")
        .expect("thread");
    sidebar.switch_thread(&thread).expect("switch");
    let state = WorkbenchState::new(DemoSource(Vec::new()), &UiSettings::default())
        .expect("state")
        .with_sidebar(sidebar);
    HeadlessWorkbench::new(state, [1200.0, 900.0])
}

fn tab(modifiers: Modifiers, pressed: bool) -> Event {
    Event::Key {
        key: Key::Tab,
        physical_key: Some(Key::Tab),
        pressed,
        repeat: false,
        modifiers,
    }
}

fn focused(draft: &str) -> (tempfile::TempDir, HeadlessWorkbench<DemoSource>) {
    let root = tempfile::tempdir().expect("root");
    let mut harness = workbench(root.path());
    harness.state_mut().composer_mut().input = draft.into();
    harness.run();
    harness.click_label("Message or /command");
    harness.run();
    (root, harness)
}

#[test]
fn tab_completes_without_toggling_role_or_moving_focus() {
    // Given: a focused draft with exactly one slash candidate.
    let (_root, mut harness) = focused("/con");
    let focus = harness.focused_id();
    // When: a backend delivers Tab together with a literal tab character.
    harness.input_mut().events.extend([
        tab(Modifiers::NONE, true),
        Event::Text("\t".into()),
        tab(Modifiers::NONE, false),
    ]);
    harness.run();
    // Then: the candidate is accepted, focus stays, and the role is untouched.
    assert_eq!(harness.state().composer().input, "/continue ");
    assert_eq!(harness.state().composer().role, ComposerRole::Worker);
    assert_eq!(harness.focused_id(), focus);
}

#[test]
fn tab_without_candidates_keeps_draft_role_and_focus() {
    for modifiers in [Modifiers::NONE, Modifiers::SHIFT] {
        let (_root, mut harness) = focused("draft");
        let focus = harness.focused_id();
        harness.input_mut().events.extend([
            tab(modifiers, true),
            Event::Text("\t".into()),
            tab(modifiers, false),
        ]);
        harness.run();
        assert_eq!(harness.state().composer().input, "draft");
        assert_eq!(harness.state().composer().role, ComposerRole::Worker);
        assert_eq!(harness.focused_id(), focus, "{modifiers:?}");
    }
}

#[test]
fn release_repeat_and_text_do_not_accept_completions() {
    let (_root, mut harness) = focused("/con");
    harness.input_mut().events.extend([
        tab(Modifiers::NONE, false),
        Event::Key {
            key: Key::Tab,
            physical_key: Some(Key::Tab),
            pressed: true,
            repeat: true,
            modifiers: Modifiers::NONE,
        },
        Event::Text("\t".into()),
        Event::Ime(egui::ImeEvent::Commit("\t".into())),
    ]);
    harness.run();
    assert_eq!(harness.state().composer().input, "/con");
}

#[test]
fn raw_hook_captures_only_plain_tabs_for_a_focused_composer() {
    let mut app = gui::app::WorkbenchApp(
        WorkbenchState::new(DemoSource(Vec::new()), &UiSettings::default()).expect("state"),
    );
    app.0.composer_mut().focused = true;
    let expected = vec![
        tab(Modifiers::CTRL, true),
        tab(Modifiers::ALT, true),
        Event::Text("keep\ttext".into()),
    ];
    let mut raw = egui::RawInput {
        events: vec![
            tab(Modifiers::NONE, true),
            tab(Modifiers::SHIFT, true),
            Event::Text("\t".into()),
            Event::Ime(egui::ImeEvent::Commit("\t".into())),
        ],
        ..Default::default()
    };
    raw.events.extend(expected.clone());
    eframe::App::raw_input_hook(&mut app, &egui::Context::default(), &mut raw);
    assert_eq!(raw.events, expected);
    assert_eq!(app.0.composer().tab_presses, [false, true]);
}

#[test]
fn raw_hook_leaves_tab_to_other_panes_and_open_settings() {
    type Setup = fn(&mut WorkbenchState<DemoSource>);
    let cases: [Setup; 7] = [
        |_| {},
        |state| WorkbenchState::open_provider_settings(state),
        |state| WorkbenchState::open_role_settings(state),
        |state| WorkbenchState::open_routing_settings(state),
        |state| WorkbenchState::open_sandbox_settings(state),
        |state| WorkbenchState::open_theme_settings(state),
        |state| WorkbenchState::open_storage_settings(state),
    ];
    for (index, setup) in cases.into_iter().enumerate() {
        let mut app = gui::app::WorkbenchApp(
            WorkbenchState::new(DemoSource(Vec::new()), &UiSettings::default()).expect("state"),
        );
        // The first case is an unfocused composer; the rest have settings open over it.
        app.0.composer_mut().focused = index > 0;
        setup(&mut app.0);
        let mut raw = egui::RawInput {
            events: vec![
                tab(Modifiers::NONE, true),
                Event::Text("\t".into()),
                tab(Modifiers::NONE, false),
                tab(Modifiers::SHIFT, true),
            ],
            ..Default::default()
        };
        let expected = raw.events.clone();
        eframe::App::raw_input_hook(&mut app, &egui::Context::default(), &mut raw);
        assert_eq!(raw.events, expected, "case {index}");
        assert!(app.0.composer().tab_presses.is_empty());
    }
}
