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

#[test]
fn role_alternates_when_tab_is_pressed_in_consecutive_frames() {
    for modifiers in [Modifiers::NONE, Modifiers::SHIFT] {
        for draft in ["draft", "/"] {
            let root = tempfile::tempdir().expect("root");
            let mut harness = workbench(root.path());
            harness.state_mut().composer_mut().input = draft.into();
            harness.run();
            harness.click_label("Message or /command");
            harness.run();
            let focus = harness.focused_id();
            for role in [
                ComposerRole::Orchestrator,
                ComposerRole::Worker,
                ComposerRole::Orchestrator,
                ComposerRole::Worker,
                ComposerRole::Orchestrator,
            ] {
                harness.input_mut().events.extend([
                    tab(modifiers, true),
                    Event::Text("\t".into()),
                    tab(modifiers, false),
                ]);
                harness.step();
                assert_eq!(harness.state().composer().role, role);
                assert!(harness.has_label(&format!("送信先: {}  (Tab で切替)", role.label())));
                assert_eq!(harness.state().composer().input, draft);
                assert_eq!(harness.focused_id(), focus);
            }
        }
    }
}

#[test]
fn both_presses_count_when_two_tabs_arrive_in_one_frame() {
    let root = tempfile::tempdir().expect("root");
    let mut harness = workbench(root.path());
    harness.run();
    for _ in 0..2 {
        harness.input_mut().events.extend([
            tab(Modifiers::NONE, true),
            Event::Text("\t".into()),
            tab(Modifiers::NONE, false),
        ]);
    }
    harness.step();
    assert_eq!(harness.state().composer().role, ComposerRole::Worker);
    assert!(harness.has_label("送信先: worker  (Tab で切替)"));
}

#[test]
fn role_is_unchanged_when_provider_settings_owns_tab() {
    let root = tempfile::tempdir().expect("root");
    let mut harness = workbench(root.path());
    harness.state_mut().open_provider_settings();
    harness.run();
    for modifiers in [Modifiers::NONE, Modifiers::SHIFT] {
        harness
            .input_mut()
            .events
            .extend([tab(modifiers, true), tab(modifiers, false)]);
        harness.step();
        assert_eq!(harness.state().composer().role, ComposerRole::Worker);
    }
}

#[test]
fn release_repeat_and_text_do_not_count_as_presses() {
    let root = tempfile::tempdir().expect("root");
    let mut harness = workbench(root.path());
    harness.state_mut().composer_mut().input = "draft".into();
    harness.run();
    harness.click_label("Message or /command");
    harness.run();
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
    harness.step();
    assert_eq!(harness.state().composer().role, ComposerRole::Worker);
    assert_eq!(harness.state().composer().input, "draft");
}

#[test]
fn raw_hook_passes_events_through_when_any_settings_is_open() {
    for open in [
        WorkbenchState::open_provider_settings,
        WorkbenchState::open_role_settings,
        WorkbenchState::open_routing_settings,
        WorkbenchState::open_sandbox_settings,
        WorkbenchState::open_theme_settings,
    ] {
        let mut app = gui::app::WorkbenchApp(
            WorkbenchState::new(DemoSource(Vec::new()), &UiSettings::default()).expect("state"),
        );
        open(&mut app.0);
        let mut raw = egui::RawInput {
            events: vec![
                tab(Modifiers::NONE, true),
                Event::Text("\t".into()),
                tab(Modifiers::NONE, false),
                tab(Modifiers::SHIFT, true),
                Event::Ime(egui::ImeEvent::Commit("\t".into())),
            ],
            ..Default::default()
        };
        let expected = raw.events.clone();
        eframe::App::raw_input_hook(&mut app, &egui::Context::default(), &mut raw);
        assert_eq!(raw.events, expected);
    }
}

#[test]
fn raw_hook_filters_only_role_shortcut_events() {
    let mut app = gui::app::WorkbenchApp(
        WorkbenchState::new(DemoSource(Vec::new()), &UiSettings::default()).expect("state"),
    );
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
}
