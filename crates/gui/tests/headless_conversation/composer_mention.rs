use egui::{Event, Key, Modifiers};
use gui::app::WorkbenchState;
use gui::fixture::DemoSource;
use gui::headless::HeadlessWorkbench;
use gui::model::commands::WorkbenchCommand;
use gui::model::composer::ProviderStatus;
use workspace_ui::{ProjectId, SidebarState, ThreadId, UiSettings};

fn workbench(root: &std::path::Path) -> HeadlessWorkbench<DemoSource> {
    for (path, content) in [
        ("src/lib.rs", ""),
        (
            ".evorch/skills/review/SKILL.md",
            "---\nname: review\ndescription: Review the change\n---\nRead every hunk.\n",
        ),
    ] {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
        std::fs::write(path, content).expect("file");
    }
    let mut sidebar = SidebarState::default();
    let project = ProjectId::new("mention-test");
    sidebar
        .add_project(project.clone(), "mention-test", root)
        .expect("project");
    sidebar.select_project(&project).expect("select");
    let thread = ThreadId::new("mention-thread");
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

fn type_text(harness: &mut HeadlessWorkbench<DemoSource>, text: &str) {
    harness.input_mut().events.push(Event::Text(text.into()));
    harness.run();
}

#[test]
fn at_mentions_complete_files_and_skills_and_send_skill_bodies() {
    // Given: a project with a source file and a repo skill, composer focused.
    let temp = tempfile::tempdir().expect("root");
    let mut harness = workbench(temp.path());
    harness.run();
    harness.click_label("Message or /command");
    harness.run();

    // When: typing `@` starts a background index of the project.
    type_text(&mut harness, "@");
    harness.state_mut().wait_mention_index();
    harness.run();
    // Then: folders, files and skills are offered together.
    for label in ["@src/", "@review"] {
        assert!(harness.has_label(label), "{label}");
    }

    // When: narrowing to the folder and accepting it with Tab keeps browsing.
    type_text(&mut harness, "sr");
    harness.key_press(Modifiers::NONE, Key::Tab);
    harness.run();
    assert_eq!(harness.state().composer().input, "@src/");
    assert!(harness.has_label("@src/lib.rs"));
    harness.key_press(Modifiers::NONE, Key::Tab);
    harness.run();
    assert_eq!(harness.state().composer().input, "@src/lib.rs ");

    // When: a skill is completed mid-sentence and the message is sent.
    type_text(&mut harness, "with @rev");
    harness.key_press(Modifiers::NONE, Key::Tab);
    harness.run();
    type_text(&mut harness, "please");
    harness.key_press(Modifiers::NONE, Key::Enter);
    harness.run();

    // Then: the agent receives the skill body while the transcript shows the typed text.
    let typed = "@src/lib.rs with @review please";
    assert!(matches!(harness.state().issued(),
        [WorkbenchCommand::SendChat(chat)]
            if chat.text == format!("{typed}\n\n<skill name=\"review\">\nRead every hunk.\n</skill>")));
    assert!(harness.has_label(&format!("You: {typed}")));
    assert!(harness.has_label(&format!("{} skill: review", gui::theme::icons::SPARKLE)));
}

#[test]
#[ignore = "writes native offscreen mention completion evidence"]
fn capture_mention_completion() {
    let temp = tempfile::tempdir().expect("root");
    let mut harness = workbench(temp.path());
    harness.run();
    harness.click_label("Message or /command");
    harness.run();
    type_text(&mut harness, "check @");
    harness.state_mut().wait_mention_index();
    harness.run();
    let evidence = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/gui-evidence/composer-mention");
    std::fs::create_dir_all(&evidence).expect("evidence directory");
    harness
        .capture()
        .expect("offscreen adapter")
        .save_png(&evidence.join("completion.png"))
        .expect("PNG evidence");
    type_text(&mut harness, "rev");
    harness.key_press(Modifiers::NONE, Key::Tab);
    harness.run();
    harness.key_press(Modifiers::NONE, Key::Enter);
    harness.run();
    harness
        .capture()
        .expect("offscreen adapter")
        .save_png(&evidence.join("sent.png"))
        .expect("PNG evidence");
}
