use super::{MockSource, sidebar_with_project, state};
use egui_kittest::{Harness, kittest::Queryable};
use gui::app::WorkbenchState;
use gui::headless::HeadlessWorkbench;
use gui::model::folder_picker::FolderPicker;
use std::path::PathBuf;
use std::sync::{Arc, mpsc};
use workspace_ui::SidebarState;

struct ScriptedFolderPicker(Result<Option<PathBuf>, String>);

impl FolderPicker for ScriptedFolderPicker {
    fn pick(&self) -> mpsc::Receiver<Result<Option<PathBuf>, String>> {
        let (tx, rx) = mpsc::channel();
        tx.send(self.0.clone()).expect("scripted selection");
        rx
    }
}

#[test]
fn add_project_expands_tilde_before_dispatch() {
    // Given: a distinct, injected home directory and an empty sidebar.
    let temp = tempfile::tempdir().expect("temp dir");
    let home = temp.path().join("home");
    let repo = home.join("repo");
    std::fs::create_dir_all(&repo).expect("repo");
    let workbench = state(MockSource::default(), SidebarState::default()).with_home_dir(home);
    let mut harness = Harness::builder()
        .with_size(egui::vec2(1000.0, 700.0))
        .build_ui_state(
            |ui, state: &mut WorkbenchState<MockSource>| {
                state.ui(ui, &mut eframe::Frame::_new_kittest())
            },
            workbench,
        );
    // When: the operator types a tilde path and presses Add project.
    harness
        .get_by_label("Project path (~ allowed)")
        .type_text("~/repo");
    harness.run();
    harness.get_by_label("Add project").click();
    harness.run();
    // Then: the registered root is the canonical home-relative path.
    assert_eq!(
        harness.state().sidebar().projects[0].repo_root,
        repo.canonicalize().expect("canonical repo")
    );
}

#[test]
fn project_row_shows_repo_root_path() {
    // Given: a registered project.
    let temp = tempfile::tempdir().expect("temp dir");
    let sidebar = sidebar_with_project(temp.path());
    let root = sidebar.projects[0].repo_root.display().to_string();
    let mut harness =
        HeadlessWorkbench::new(state(MockSource::default(), sidebar), [1000.0, 700.0]);
    // When: the sidebar renders.
    harness.run();
    // Then: the project's root is exposed under its name.
    assert!(harness.has_label(&root));
}

#[test]
fn browse_button_dispatches_picker_and_adds_selected_folder() {
    // Given: a picker returning an existing folder and sidebar persistence.
    let temp = tempfile::tempdir().expect("temp dir");
    let save = temp.path().join("sidebar.json");
    let workbench = state(MockSource::default(), SidebarState::default())
        .with_sidebar_path(save.clone())
        .with_folder_picker(Arc::new(ScriptedFolderPicker(Ok(Some(temp.path().into())))));
    let mut harness = HeadlessWorkbench::new(workbench, [1000.0, 700.0]);
    harness.run();
    // When: Browse dispatches the picker and a following frame consumes it.
    harness.click_label("Browse…");
    harness.run();
    // Then: the selected project is added and persisted.
    assert_eq!(
        harness.state().sidebar().projects[0].repo_root,
        temp.path().canonicalize().expect("root")
    );
    assert_eq!(
        workspace_ui::load_sidebar(&save)
            .expect("saved sidebar")
            .projects
            .len(),
        1
    );
}

#[test]
fn picker_error_surfaces_in_sidebar() {
    // Given: an unavailable portal.
    let workbench = state(MockSource::default(), SidebarState::default()).with_folder_picker(
        Arc::new(ScriptedFolderPicker(Err("portal unavailable".into()))),
    );
    let mut harness = HeadlessWorkbench::new(workbench, [1000.0, 700.0]);
    harness.run();
    // When: the operator tries Browse.
    harness.click_label("Browse…");
    harness.run();
    // Then: the error is visible without adding a project.
    assert!(harness.has_label("portal unavailable"));
    assert!(harness.state().sidebar().projects.is_empty());
}

#[test]
fn allowed_directories_section_hidden_when_empty_and_collapsed_when_present() {
    // Given: a selected project with no external directories.
    let temp = tempfile::tempdir().expect("temp dir");
    let allowed = tempfile::tempdir().expect("allowed dir");
    let mut harness = HeadlessWorkbench::new(
        state(MockSource::default(), sidebar_with_project(temp.path())),
        [1000.0, 700.0],
    );
    harness.run();
    assert!(!harness.has_label("Allowed directories"));
    assert!(!harness.has_label("Allowed directories (0)"));
    harness
        .state_mut()
        .add_allowed_directory(allowed.path())
        .expect("allowed");
    harness.run();
    let path = allowed
        .path()
        .canonicalize()
        .expect("canonical path")
        .display()
        .to_string();
    assert!(harness.has_label("Allowed directories (1)"));
    assert!(!harness.has_label(&path));
    // When: the counted disclosure is expanded.
    harness.click_label("Allowed directories (1)");
    harness.run();
    // Then: its path is exposed.
    assert!(harness.has_label(&path));
}
