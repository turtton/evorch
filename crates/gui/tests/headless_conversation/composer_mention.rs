use egui::{Event, Key, Modifiers};
use gui::app::WorkbenchState;
use gui::fixture::DemoSource;
use gui::headless::HeadlessWorkbench;
use gui::model::commands::WorkbenchCommand;
use gui::model::composer::{CompletionKind, ProviderStatus};
use gui::model::transcript::TranscriptEntry;
use workspace_ui::{ProjectId, SidebarState, ThreadId, TrustState, UiSettings};

fn workbench(
    root: &std::path::Path,
    allowed: &[&std::path::Path],
    trust: TrustState,
) -> HeadlessWorkbench<DemoSource> {
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
    sidebar.set_project_trust(&project, trust).expect("trust");
    for directory in allowed {
        sidebar
            .add_allowed_directory(&project, directory, TrustState::Approved)
            .expect("allowed directory");
    }
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

/// The `/`-separated absolute form completions insert for a directory.
fn slash_form(path: &std::path::Path) -> String {
    path.to_str()
        .expect("utf-8 temp path")
        .replace(std::path::MAIN_SEPARATOR, "/")
}

fn completion_labels(harness: &HeadlessWorkbench<DemoSource>) -> Vec<String> {
    harness
        .state()
        .composer()
        .completion_items()
        .into_iter()
        .map(|item| item.label)
        .collect()
}

/// The skills offered for the mention being typed.
fn skill_labels(harness: &HeadlessWorkbench<DemoSource>) -> Vec<String> {
    harness
        .state()
        .composer()
        .completion_items()
        .into_iter()
        .filter(|item| item.kind == CompletionKind::Skill)
        .map(|item| item.label)
        .collect()
}

/// Writes skill `name` into the skills directory `dir` of `root`.
fn write_skill(root: &std::path::Path, dir: &str, name: &str, body: &str) {
    let skill = root.join(dir).join(name);
    std::fs::create_dir_all(&skill).expect("skill dir");
    std::fs::write(
        skill.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: {name} fixture\n---\n{body}\n"),
    )
    .expect("SKILL.md");
}

/// A skill in each repository scope, named so no user skill can stand in for it.
fn write_untrusted_fixture_skills(root: &std::path::Path) {
    write_skill(
        root,
        ".evorch/skills",
        "evorch-fixture-skill",
        "EVORCH FIXTURE BODY",
    );
    write_skill(
        root,
        ".agents/skills",
        "agents-fixture-skill",
        "AGENTS FIXTURE BODY",
    );
}

#[test]
fn at_mentions_complete_files_and_skills_and_send_skill_bodies() {
    // Given: a trusted project with a source file and a repo skill, composer focused.
    let temp = tempfile::tempdir().expect("root");
    let mut harness = workbench(temp.path(), &[], TrustState::Approved);
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

    // When: Tab and Shift+Tab cycle the candidates without editing the draft.
    type_text(&mut harness, "sr");
    harness.key_press(Modifiers::NONE, Key::Tab);
    harness.run();
    assert_eq!(harness.state().composer().completion_selected, 1);
    harness.key_press(Modifiers::SHIFT, Key::Tab);
    harness.run();
    assert_eq!(harness.state().composer().completion_selected, 0);
    assert_eq!(harness.state().composer().input, "@sr");

    // When: accepting the highlighted folder keeps browsing.
    harness.key_press(Modifiers::NONE, Key::Enter);
    harness.run();
    assert_eq!(harness.state().composer().input, "@src/");
    assert!(harness.has_label("@src/lib.rs"));
    harness.key_press(Modifiers::NONE, Key::Tab);
    harness.run();
    assert_eq!(harness.state().composer().input, "@src/lib.rs ");

    // When: a skill is completed mid-sentence and the message is sent.
    type_text(&mut harness, "with @rev");
    harness.key_press(Modifiers::NONE, Key::Enter);
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
fn untrusted_projects_offer_builtin_skills_but_not_their_repo_skills() {
    // Given: an untrusted project with a skill in each repository scope, composer focused.
    let temp = tempfile::tempdir().expect("root");
    write_untrusted_fixture_skills(temp.path());
    let mut harness = workbench(temp.path(), &[], TrustState::Unapproved);
    harness.run();
    harness.click_label("Message or /command");
    harness.run();

    // When: a builtin skill is searched for.
    type_text(&mut harness, "use @git-best");
    harness.state_mut().wait_mention_index();
    harness.run();
    // Then: it is still offered.
    assert!(skill_labels(&harness).contains(&"@git-best-practices".to_owned()));

    // When: each repository skill is searched for.
    for (typed, name) in [
        ("-practices with @evorch-fixture", "@evorch-fixture-skill"),
        ("-skill and @agents-fixture", "@agents-fixture-skill"),
    ] {
        type_text(&mut harness, typed);
        harness.state_mut().wait_mention_index();
        harness.run();
        // Then: neither is offered, since sending would not inline it.
        assert!(!skill_labels(&harness).contains(&name.to_owned()), "{name}");
    }
}

#[test]
fn untrusted_projects_send_repo_skill_mentions_as_plain_text_with_a_notice() {
    // Given: an untrusted project with a skill in each repository scope, composer focused.
    let temp = tempfile::tempdir().expect("root");
    write_untrusted_fixture_skills(temp.path());
    let mut harness = workbench(temp.path(), &[], TrustState::Unapproved);
    harness.run();
    harness.click_label("Message or /command");
    harness.run();

    // When: a message naming a builtin skill and both repository skills is sent.
    let typed =
        "use @git-best-practices with @evorch-fixture-skill and @agents-fixture-skill please";
    type_text(&mut harness, typed);
    harness.key_press(Modifiers::NONE, Key::Enter);
    harness.run();

    // Then: it is sent with only the builtin skill inlined; the repository
    // skills stay plain text.
    let [WorkbenchCommand::SendChat(chat)] = harness.state().issued() else {
        panic!("one chat is sent: {:?}", harness.state().issued());
    };
    assert!(chat.text.starts_with(typed), "{}", chat.text);
    assert!(chat.text.contains("<skill name=\"git-best-practices\">"));
    for leaked in ["fixture-skill\">", "FIXTURE BODY"] {
        assert!(!chat.text.contains(leaked), "{leaked}: {}", chat.text);
    }
    // Then: a notice below the message names them, says why and how to enable them.
    let notice = "Sent @evorch-fixture-skill, @agents-fixture-skill as plain text — this project \
                  is not trusted, so its own skills are not expanded. Enable \"Trust project \
                  instructions\" in Project settings to use them.";
    let entries = harness.state().transcript().entries();
    let message = entries
        .iter()
        .position(|entry| matches!(entry, TranscriptEntry::UserMessage { .. }))
        .expect("user message");
    assert!(
        entries[message..]
            .iter()
            .any(|entry| matches!(entry, TranscriptEntry::Notice { text } if text == notice)),
        "{entries:?}"
    );
    assert!(harness.has_label(notice));
}

#[test]
fn granting_or_revoking_trust_updates_offered_repo_skills_at_once() {
    // Given: a trusted project whose repository skill completes in an open mention.
    let temp = tempfile::tempdir().expect("root");
    write_skill(
        temp.path(),
        ".evorch/skills",
        "evorch-fixture-skill",
        "EVORCH FIXTURE BODY",
    );
    let mut harness = workbench(temp.path(), &[], TrustState::Approved);
    harness.run();
    harness.click_label("Message or /command");
    harness.run();
    type_text(&mut harness, "@evorch-fixture");
    harness.state_mut().wait_mention_index();
    harness.run();
    let skill = "@evorch-fixture-skill".to_owned();
    assert!(skill_labels(&harness).contains(&skill));
    let project = ProjectId::new("mention-test");

    // When: the project's trust is revoked while the mention stays open.
    harness
        .state_mut()
        .set_project_trust(&project, TrustState::Unapproved)
        .expect("revoke");
    harness.step();
    // Then: the very next frame no longer offers it, nor does the rebuild.
    assert!(!skill_labels(&harness).contains(&skill));
    harness.state_mut().wait_mention_index();
    harness.run();
    assert!(!skill_labels(&harness).contains(&skill));

    // When: trust is granted again.
    harness
        .state_mut()
        .set_project_trust(&project, TrustState::Approved)
        .expect("grant");
    harness.step();
    harness.state_mut().wait_mention_index();
    harness.run();
    // Then: the rebuilt index offers it again.
    assert!(skill_labels(&harness).contains(&skill));
}

#[test]
fn at_mentions_complete_files_in_allowed_directories_as_absolute_paths() {
    // Given: a project with an approved allowed directory outside its root.
    let temp = tempfile::tempdir().expect("root");
    let shared = tempfile::tempdir().expect("shared");
    let shared_root = shared.path().canonicalize().expect("canonical shared");
    std::fs::create_dir_all(shared_root.join("notes")).expect("notes");
    std::fs::write(shared_root.join("notes/plan.md"), "").expect("plan");
    let mut harness = workbench(temp.path(), &[&shared_root], TrustState::Approved);
    harness.run();
    harness.click_label("Message or /command");
    harness.run();
    let base = slash_form(&shared_root);

    // When: a mention starts, the index covers the allowed directory too.
    type_text(&mut harness, "@");
    harness.state_mut().wait_mention_index();
    harness.run();
    // Then: the allowed directory is offered beside the project root.
    for label in ["@src/".to_owned(), format!("@{base}/")] {
        assert!(harness.has_label(&label), "{label}");
    }

    // When: a file inside it is searched by name and accepted.
    type_text(&mut harness, "plan");
    harness.state_mut().wait_mention_index();
    harness.run();
    assert!(harness.has_label(&format!("@{base}/notes/plan.md")));
    harness.key_press(Modifiers::NONE, Key::Enter);
    harness.run();

    // Then: the draft holds the absolute path the agent was told about.
    assert_eq!(
        harness.state().composer().input,
        format!("@{base}/notes/plan.md ")
    );
}

#[test]
fn revoked_or_removed_allowed_directories_stop_completing_at_once() {
    // Given: two approved allowed directories completing during an open mention.
    let temp = tempfile::tempdir().expect("root");
    let shared = tempfile::tempdir().expect("shared");
    let allowed_dir = |name: &str, file: &str| {
        let root = shared.path().join(name);
        std::fs::create_dir_all(&root).expect("allowed directory");
        std::fs::write(root.join(file), "").expect("file");
        root.canonicalize().expect("canonical")
    };
    let kept = allowed_dir("kept", "keep.md");
    let revoked = allowed_dir("revoked", "drop.md");
    let mut harness = workbench(temp.path(), &[&kept, &revoked], TrustState::Approved);
    harness.run();
    harness.click_label("Message or /command");
    harness.run();
    type_text(&mut harness, "@.md");
    harness.state_mut().wait_mention_index();
    harness.run();
    let keep = format!("@{}/keep.md", slash_form(&kept));
    let dropped = format!("@{}/drop.md", slash_form(&revoked));
    for label in [&keep, &dropped] {
        assert!(harness.has_label(label), "{label}");
    }
    let project = ProjectId::new("mention-test");

    // When: one directory's approval is withdrawn.
    harness
        .state_mut()
        .set_allowed_trust(&project, &revoked, TrustState::Unapproved)
        .expect("revoke");
    harness.step();
    // Then: the very next frame no longer offers it, without waiting for a rebuild.
    assert!(!completion_labels(&harness).contains(&dropped));
    assert!(completion_labels(&harness).contains(&keep));

    // When: the other directory is removed from the project.
    harness
        .state_mut()
        .remove_allowed_directory(&project, &kept)
        .expect("remove");
    harness.step();
    // Then: it stops completing as well.
    assert!(!completion_labels(&harness).contains(&keep));
    harness.run();
    for label in [&keep, &dropped] {
        assert!(!harness.has_label(label), "{label}");
    }
}

#[test]
#[ignore = "writes native offscreen mention completion evidence"]
fn capture_mention_completion() {
    let temp = tempfile::tempdir().expect("root");
    let mut harness = workbench(temp.path(), &[], TrustState::Approved);
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
