use std::path::{Path, PathBuf};

use egui::OutputCommand;
use egui_kittest::{
    Harness,
    kittest::{NodeT, Queryable},
};

use super::super::render_markdown_with_base;

fn project() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    std::fs::create_dir(root.join("src")).unwrap();
    std::fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
    (dir, root)
}

fn harness(source: &str, root: &Path) -> Harness<'static> {
    let (source, root) = (source.to_owned(), root.to_path_buf());
    let mut harness = Harness::new_ui(move |ui| {
        render_markdown_with_base(ui, &source, "link-menu", Some(&root), Some(&root));
    });
    harness.run_steps(2);
    harness
}

fn choose(harness: &mut Harness<'_>, link: &str, item: &str) -> Vec<OutputCommand> {
    harness.get_by_label(link).click_secondary();
    harness.run();
    harness.get_by_label(item).click();
    harness.step();
    harness.output().platform_output.commands.clone()
}

fn copied(commands: &[OutputCommand]) -> Option<&str> {
    commands.iter().find_map(|command| match command {
        OutputCommand::CopyText(text) => Some(text.as_str()),
        _ => None,
    })
}

#[test]
fn file_link_menu_opens_and_copies_repo_relative_and_absolute_paths() {
    // Given: an agent message linking a project file with a line anchor.
    let (_dir, root) = project();
    let mut harness = harness("See [main](src/main.rs#L3).", &root);

    // When / Then: Open takes the same route as a left click.
    let commands = choose(&mut harness, "main", "Open");
    assert!(commands.iter().any(
        |command| matches!(command, OutputCommand::OpenUrl(url) if url.url == "src/main.rs#L3")
    ));

    // When / Then: copies drop the anchor and use the requested form.
    let commands = choose(&mut harness, "main", "Copy relative path");
    assert_eq!(copied(&commands), Some("src/main.rs"));
    let commands = choose(&mut harness, "main", "Copy absolute path");
    let absolute = root.join("src/main.rs").display().to_string();
    assert_eq!(copied(&commands), Some(absolute.as_str()));
}

#[test]
fn file_link_outside_the_project_only_copies_its_absolute_path() {
    // Given: a link to an absolute path outside the project root.
    let (_dir, root) = project();
    let (_other, outside) = project();
    let target = outside.join("src/main.rs").display().to_string();
    let mut harness = harness(&format!("[elsewhere]({target})"), &root);

    // When: its menu opens.
    harness.get_by_label("elsewhere").click_secondary();
    harness.run();
    // Then: no relative form exists, but the absolute path can be copied.
    assert!(
        harness
            .get_by_label("Copy relative path")
            .accesskit_node()
            .is_disabled()
    );
    harness.get_by_label("Copy absolute path").click();
    harness.step();
    assert_eq!(
        copied(&harness.output().platform_output.commands),
        Some(target.as_str())
    );
}

#[test]
fn external_link_menu_opens_in_browser_and_copies_url() {
    // Given: a web link.
    let (_dir, root) = project();
    let mut harness = harness("[docs](https://example.com/guide)", &root);

    // When / Then: no path entries; the URL is opened or copied verbatim.
    let commands = choose(&mut harness, "docs", "Copy URL");
    assert_eq!(copied(&commands), Some("https://example.com/guide"));
    harness.get_by_label("docs").click_secondary();
    harness.run();
    assert!(harness.query_by_label("Copy relative path").is_none());
    harness.get_by_label("Open in browser").click();
    harness.step();
    assert!(harness.output().platform_output.commands.iter().any(
        |command| matches!(command, OutputCommand::OpenUrl(url) if url.url == "https://example.com/guide")
    ));
}

#[test]
fn table_cell_links_share_the_link_menu() {
    // Given: a file link inside a rendered table cell.
    let (_dir, root) = project();
    let mut harness = harness("| File |\n| --- |\n| [cell](src/main.rs) |\n", &root);

    // When / Then: the same copy actions are available there.
    let commands = choose(&mut harness, "cell", "Copy relative path");
    assert_eq!(copied(&commands), Some("src/main.rs"));
}
