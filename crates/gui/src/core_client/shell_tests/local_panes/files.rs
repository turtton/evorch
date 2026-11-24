use super::*;
use crate::panes::file_viewer::wait_for_file_for_tests;
use egui_kittest::kittest::NodeT;

fn write_lines(path: &Path, prefix: &str) {
    let text: String = (1..=200)
        .map(|line| format!("// {prefix} {line}\n"))
        .collect();
    std::fs::write(path, text).unwrap();
}

/// The window on thr-1 with proj-1 rooted in `project`, and `text` as the last
/// assistant message.
fn screen_on(project: &Path, text: &str) -> Screen {
    let mut screen = screen(egui::vec2(1440.0, 900.0));
    screen.settle_with(&roots(vec![("proj-1", project.into())], Vec::new()));
    screen
        .shared
        .lock()
        .unwrap()
        .cache
        .history
        .get_mut("thr-1")
        .unwrap()
        .push(message(5, "assistant", text));
    screen.harness.run_steps(2);
    screen
}

impl Screen {
    fn file_tabs(&self) -> Vec<CoreTab> {
        self.harness
            .state()
            .dock
            .iter_all_tabs()
            .map(|(_, tab)| *tab)
            .filter(|tab| matches!(tab, CoreTab::File(_)))
            .collect()
    }

    /// Clicks the link `label` and draws the file it opens once it has been read.
    fn open_link(&mut self, label: &str, path: &Path) {
        self.harness.get_by_label(label).click();
        self.harness.run_steps(2);
        wait_for_file_for_tests(&self.harness.ctx, path);
        self.harness.run_steps(2);
    }
}

// Given: proj-1's root holds source.rs, linked from an assistant message by a relative
//        path with a line, and again with ./ and an anchor
// When: the first link is clicked, then the second
// Then: one source.rs tab opens right after Execution, in front, at line 120; the
//       second link brings the same tab to line 150
#[test]
fn a_relative_link_in_an_assistant_message_opens_a_file_tab_at_its_line() {
    let project = tempfile::tempdir().unwrap();
    let file = project.path().canonicalize().unwrap().join("source.rs");
    write_lines(&file, "line");
    let mut screen = screen_on(
        project.path(),
        "[Open source](source.rs:120)\n\n[Open again](./source.rs#L150)",
    );

    screen.open_link("Open source", &file);
    let [tab] = screen.file_tabs()[..] else {
        panic!("one File tab");
    };
    screen.harness.get_by_label("source.rs tab");
    screen.harness.get_by_label("// line 120");
    let dock = &screen.harness.state().dock;
    let opened = dock.find_tab(&tab).unwrap();
    let execution = dock
        .find_tab(&CoreTab::Feature(FeatureId::Execution))
        .unwrap();
    assert_eq!(opened.node_path(), execution.node_path());
    assert_eq!(opened.tab.0, execution.tab.0 + 1);
    assert_eq!(screen.front_of_leaf(tab), tab);

    screen.open_link("Open again", &file);
    assert_eq!(screen.file_tabs(), [tab]);
    screen.harness.get_by_label("// line 150");
    screen.capture("file");
}

// Given: a file:// link with a line anchor and an absolute path with a line, both to
//        source.rs
// When: both are clicked
// Then: they share one tab
#[test]
fn a_file_uri_and_an_absolute_path_open_the_same_tab() {
    let project = tempfile::tempdir().unwrap();
    let file = project.path().canonicalize().unwrap().join("source.rs");
    write_lines(&file, "line");
    let uri = url::Url::from_file_path(&file).unwrap();
    let mut screen = screen_on(
        project.path(),
        &format!("[By URI]({uri}#L8)\n\n[By path]({}:5)", file.display()),
    );

    screen.open_link("By URI", &file);
    screen.harness.get_by_label("// line 8");
    screen.open_link("By path", &file);
    assert_eq!(screen.file_tabs().len(), 1);
    screen.harness.get_by_label("// line 5");
}

// Given: a web link and a mail link in an assistant message
// When: each is clicked
// Then: each is left for the desktop to open, and no File tab opens
#[test]
fn other_urls_pass_through_to_the_desktop() {
    let project = tempfile::tempdir().unwrap();
    let mut screen = screen_on(
        project.path(),
        "[Docs](https://example.com/docs)\n\n[Mail](mailto:me@example.com)",
    );

    for (label, url) in [
        ("Docs", "https://example.com/docs"),
        ("Mail", "mailto:me@example.com"),
    ] {
        screen.harness.get_by_label(label).click();
        screen.harness.step();
        let opened = screen.harness.output().platform_output.commands.iter().any(
            |command| matches!(command, egui::OutputCommand::OpenUrl(open) if open.url == url),
        );
        assert!(opened, "{label} was not handed to the desktop");
    }
    assert!(screen.file_tabs().is_empty());
}

// Given: docs/readme.md links nearby.rs, and an assistant message links the document
// When: the document is opened and its link clicked in the preview
// Then: the link resolves in docs/, beside the document, not in proj-1's root
#[test]
fn markdown_links_in_a_file_tab_resolve_against_its_directory() {
    let project = tempfile::tempdir().unwrap();
    let docs = project.path().canonicalize().unwrap().join("docs");
    std::fs::create_dir(&docs).unwrap();
    std::fs::write(
        docs.join("readme.md"),
        "# File preview\n\n[Related source](nearby.rs)",
    )
    .unwrap();
    std::fs::write(docs.join("nearby.rs"), "fn nearby() {}\n").unwrap();
    let mut screen = screen_on(project.path(), "[Open document](docs/readme.md)");

    screen.open_link("Open document", &docs.join("readme.md"));
    screen.harness.get_by_label("File preview");
    screen.open_link("Related source", &docs.join("nearby.rs"));

    screen.harness.get_by_label("nearby.rs tab");
    screen.harness.get_by_label("fn nearby() {}");
    assert_eq!(screen.file_tabs().len(), 2);
}

// Given: source.rs open in a File tab
// When: the tab is closed with a middle click, the file rewritten and opened again,
//       and then the layout is reset
// Then: the closed tab is dropped with its preview, so the reopened tab shows the new
//       text at once; a reset drops the tab it finds
#[test]
fn closing_a_file_tab_forgets_it_and_reset_layout_drops_file_tabs() {
    let project = tempfile::tempdir().unwrap();
    let file = project.path().canonicalize().unwrap().join("source.rs");
    write_lines(&file, "line");
    let mut screen = screen_on(project.path(), "[Open source](source.rs:120)");
    screen.open_link("Open source", &file);
    screen.harness.get_by_label("// line 120");

    screen
        .harness
        .get_by_label("source.rs tab")
        .click_button(egui::PointerButton::Middle);
    screen.harness.run_steps(2);
    assert!(screen.file_tabs().is_empty());
    assert!(screen.harness.state().local.files.is_empty());

    write_lines(&file, "rewritten");
    screen.harness.get_by_label("Open source").click();
    screen.harness.run_steps(2);
    assert!(screen.harness.query_by_label("// line 120").is_none());
    wait_for_file_for_tests(&screen.harness.ctx, &file);
    screen.harness.run_steps(2);
    screen.harness.get_by_label("// rewritten 120");

    screen.harness.get_by_label("⚙").click();
    screen.harness.run_steps(2);
    screen.harness.get_by_label("Reset layout").click();
    screen.harness.run_steps(2);
    assert!(screen.file_tabs().is_empty());
    assert!(screen.harness.state().local.files.is_empty());
}

// Given: proj-1's root missing on this machine, so relative links have no base
// When: a relative link is clicked
// Then: it is neither opened in a tab nor handed to the desktop, which would resolve
//       it against this process's directory
#[test]
fn a_relative_link_without_a_workspace_is_not_handed_to_the_desktop() {
    let dir = tempfile::tempdir().unwrap();
    let mut screen = screen_on(&dir.path().join("gone"), "[Open source](source.rs:120)");

    screen.harness.get_by_label("Open source").click();
    screen.harness.step();

    let commands = &screen.harness.output().platform_output.commands;
    assert!(
        !commands
            .iter()
            .any(|command| matches!(command, egui::OutputCommand::OpenUrl(_))),
        "{commands:?}"
    );
    screen.harness.run_steps(2);
    assert!(screen.file_tabs().is_empty());
}

// Given: proj-1's root missing on this machine
// When: a relative link's menu opens
// Then: it offers no browser to open it in, says why it cannot open, and still copies
//       the link
#[test]
fn a_relative_link_menu_without_a_workspace_says_it_cannot_open() {
    let dir = tempfile::tempdir().unwrap();
    let mut screen = screen_on(&dir.path().join("gone"), "[Open source](source.rs:120)");

    screen.harness.get_by_label("Open source").click_secondary();
    screen.harness.run_steps(2);

    assert!(screen.harness.query_by_label("Open in browser").is_none());
    assert!(
        screen
            .harness
            .get_by_label("Open")
            .accesskit_node()
            .is_disabled()
    );
    screen.harness.get_by_label("Copy link").click();
    screen.harness.step();
    let commands = &screen.harness.output().platform_output.commands;
    assert!(
        commands.iter().any(
            |command| matches!(command, egui::OutputCommand::CopyText(text) if text == "source.rs:120")
        ),
        "{commands:?}"
    );
    assert!(
        !commands
            .iter()
            .any(|command| matches!(command, egui::OutputCommand::OpenUrl(_))),
        "{commands:?}"
    );
}

// Given: proj-1 and a sibling directory outside it, which holds outside.rs
// When: an assistant message links outside.rs by climbing out of proj-1 with `..`, the
//       sibling directory by its absolute path, and a file on another host
// Then: as in the embedded workbench, no link is held to proj-1: outside.rs opens at its
//       line; the directory opens but is refused as not a regular file; the other
//       host's file is not read here and is left for the desktop
#[test]
fn links_outside_the_project_follow_the_embedded_workbench_rules() {
    let dir = tempfile::tempdir().unwrap();
    let common = dir.path().canonicalize().unwrap();
    let project = common.join("project");
    let elsewhere = common.join("elsewhere");
    std::fs::create_dir(&project).unwrap();
    std::fs::create_dir(&elsewhere).unwrap();
    let outside = elsewhere.join("outside.rs");
    write_lines(&outside, "outside");
    let mut screen = screen_on(
        &project,
        &format!(
            "[Climb out](../elsewhere/outside.rs:40)\n\n[Folder]({})\n\n[Remote](file://otherhost/etc/hosts)",
            elsewhere.display()
        ),
    );

    screen.open_link("Climb out", &outside);
    screen.harness.get_by_label("outside.rs tab");
    screen.harness.get_by_label("// outside 40");

    screen.open_link("Folder", &elsewhere);
    screen.harness.get_by_label("elsewhere tab");
    screen.harness.get_by_label("Select a regular file.");
    assert_eq!(screen.file_tabs().len(), 2);

    screen.harness.get_by_label("Remote").click();
    screen.harness.step();
    let commands = &screen.harness.output().platform_output.commands;
    assert!(
        commands.iter().any(|command| matches!(command,
            egui::OutputCommand::OpenUrl(open) if open.url == "file://otherhost/etc/hosts")),
        "{commands:?}"
    );
    screen.harness.run_steps(2);
    assert_eq!(screen.file_tabs().len(), 2);
}

// Given: a PNG in proj-1's root, linked from an assistant message
// When: the link is clicked
// Then: a shot.png tab opens in front and draws the picture, not its bytes
#[test]
fn an_image_link_opens_the_image_in_a_file_tab() {
    let project = tempfile::tempdir().unwrap();
    let file = project.path().canonicalize().unwrap().join("shot.png");
    image::RgbImage::from_fn(480, 300, |x, y| {
        image::Rgb([(x * 255 / 479) as u8, (y * 255 / 299) as u8, 160])
    })
    .save(&file)
    .unwrap();
    let mut screen = screen_on(project.path(), "[Screenshot](shot.png)");

    screen.open_link("Screenshot", &file);
    let [tab] = screen.file_tabs()[..] else {
        panic!("one File tab");
    };
    screen.harness.get_by_label("shot.png tab");
    assert_eq!(screen.front_of_leaf(tab), tab);
    let name = file.display().to_string();
    let drawn = screen
        .harness
        .ctx
        .tex_manager()
        .read()
        .allocated()
        .any(|(_, meta)| meta.name == name);
    assert!(drawn, "the picture was not uploaded");
    assert!(screen.harness.query_by_label("Loading file…").is_none());
    screen.capture("image");
}

// Given: a Markdown file open in a File tab in Graphite, whose selection stroke is its fill
// When: the tab shows the Source/Preview toggle with Preview selected
// Then: the selected label is drawn in the accent's foreground, so it reads on the accent
#[test]
fn the_selected_preview_toggle_is_readable_in_graphite() {
    let project = tempfile::tempdir().unwrap();
    let file = project.path().canonicalize().unwrap().join("notes.md");
    std::fs::write(&file, "# Notes\n\nBody text.\n").unwrap();
    let mut screen = screen_on(project.path(), "[Open notes](notes.md)");
    screen.open_link("Open notes", &file);
    screen.harness.get_by_label("Notes");

    let [selected] = screen
        .harness
        .get_all_by_label("Preview")
        .filter(|node| node.accesskit_node().toggled() == Some(egui::accesskit::Toggled::True))
        .map(|node| node.rect())
        .collect::<Vec<_>>()[..]
    else {
        panic!("one selected Preview toggle");
    };
    let palette = crate::theme::tokens::palette();
    let shapes = &screen.harness.output().shapes;
    let filled = shapes.iter().any(|clipped| {
        matches!(&clipped.shape, egui::Shape::Rect(rect)
            if rect.fill == palette.ACCENT && selected.contains_rect(rect.rect))
    });
    let label = shapes.iter().find_map(|clipped| match &clipped.shape {
        egui::Shape::Text(text)
            if text.galley.text() == "Preview"
                && selected.contains(text.galley.rect.translate(text.pos.to_vec2()).center()) =>
        {
            Some(text.override_text_color.unwrap_or(text.fallback_color))
        }
        _ => None,
    });
    assert!(filled, "the selected pill is filled with the accent");
    assert_eq!(
        label,
        Some(palette.ACCENT_FG),
        "the label reads on the fill"
    );
    screen.capture("file-toggle-graphite");
}
