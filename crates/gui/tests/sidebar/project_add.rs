use super::{MockSource, sidebar_with_project, state};
use egui_kittest::{Harness, kittest::Queryable};
use gui::app::WorkbenchState;
use gui::headless::HeadlessWorkbench;
use gui::model::folder_picker::FolderPicker;
use gui::model::project_dialog::ProjectDialog;
use gui::panes::project_dialog::{NAME_LABEL, PATH_LABEL};
use std::path::PathBuf;
use std::sync::{Arc, mpsc};
use workspace_ui::{ProjectId, SidebarState};

struct ScriptedFolderPicker(Result<Option<PathBuf>, String>);

impl FolderPicker for ScriptedFolderPicker {
    fn pick(&self) -> mpsc::Receiver<Result<Option<PathBuf>, String>> {
        let (tx, rx) = mpsc::channel();
        tx.send(self.0.clone()).expect("scripted selection");
        rx
    }
}

fn typing_harness(
    workbench: WorkbenchState<MockSource>,
) -> Harness<'static, WorkbenchState<MockSource>> {
    Harness::builder()
        .with_size(egui::vec2(1000.0, 700.0))
        .build_ui_state(
            |ui, state: &mut WorkbenchState<MockSource>| {
                state.ui(ui, &mut eframe::Frame::_new_kittest())
            },
            workbench,
        )
}

fn type_into(harness: &mut Harness<'static, WorkbenchState<MockSource>>, label: &str, text: &str) {
    harness.get_by_label(label).click();
    harness.run_steps(4);
    harness.get_by_label(label).type_text(text);
    harness.run_steps(4);
}

fn click(harness: &mut Harness<'static, WorkbenchState<MockSource>>, label: &str) {
    harness.get_by_label(label).click();
    harness.run_steps(4);
}

#[test]
fn add_modal_expands_tilde_registers_persists_and_closes() {
    // Given: a distinct, injected home directory and an empty, persisted sidebar.
    let temp = tempfile::tempdir().expect("temp dir");
    let home = temp.path().join("home");
    let repo = home.join("repo");
    std::fs::create_dir_all(&repo).expect("repo");
    let save = temp.path().join("sidebar.json");
    let workbench = state(MockSource::default(), SidebarState::default())
        .with_home_dir(home)
        .with_sidebar_path(save.clone());
    let mut harness = typing_harness(workbench);
    // When: the operator opens the add modal, types a tilde path and presses Add.
    click(&mut harness, "Add project");
    type_into(&mut harness, PATH_LABEL, "~/repo");
    click(&mut harness, "Add");
    // Then: the canonical home-relative root is registered, persisted and the modal closes.
    let canonical = repo.canonicalize().expect("canonical repo");
    assert_eq!(harness.state().sidebar().projects[0].repo_root, canonical);
    assert_eq!(
        workspace_ui::load_sidebar(&save)
            .expect("saved sidebar")
            .projects[0]
            .repo_root,
        canonical
    );
    assert_eq!(harness.state().project_dialog(), &ProjectDialog::Closed);
}

#[test]
fn add_modal_keeps_input_and_shows_error_when_registration_fails() {
    // Given: an empty sidebar and a path that does not exist.
    let temp = tempfile::tempdir().expect("temp dir");
    let missing = temp.path().join("missing").display().to_string();
    let mut harness = typing_harness(state(MockSource::default(), SidebarState::default()));
    // When: the operator submits the missing path.
    click(&mut harness, "Add project");
    type_into(&mut harness, PATH_LABEL, &missing);
    click(&mut harness, "Add");
    // Then: nothing is registered and the modal stays open with the typed path and an error.
    assert!(harness.state().sidebar().projects.is_empty());
    let ProjectDialog::Add { path, error } = harness.state().project_dialog() else {
        panic!("add modal must stay open");
    };
    assert_eq!(path, &missing);
    assert!(error.is_some());
}

#[test]
fn browse_fills_add_modal_and_add_registers_selected_folder() {
    // Given: a picker returning an existing folder and sidebar persistence.
    let temp = tempfile::tempdir().expect("temp dir");
    let save = temp.path().join("sidebar.json");
    let workbench = state(MockSource::default(), SidebarState::default())
        .with_sidebar_path(save.clone())
        .with_folder_picker(Arc::new(ScriptedFolderPicker(Ok(Some(temp.path().into())))));
    let mut harness = HeadlessWorkbench::new(workbench, [1000.0, 700.0]);
    harness.run();
    harness.click_label("Add project");
    harness.run();
    // When: Browse resolves to a folder.
    harness.click_label("Browse…");
    harness.run();
    // Then: the folder only fills the form until Add confirms it.
    assert!(harness.state().sidebar().projects.is_empty());
    assert_eq!(
        harness.state().project_dialog(),
        &ProjectDialog::Add {
            path: temp.path().display().to_string(),
            error: None,
        }
    );
    harness.click_label("Add");
    harness.run();
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
fn picker_error_surfaces_in_add_modal() {
    // Given: an unavailable portal.
    let workbench = state(MockSource::default(), SidebarState::default()).with_folder_picker(
        Arc::new(ScriptedFolderPicker(Err("portal unavailable".into()))),
    );
    let mut harness = HeadlessWorkbench::new(workbench, [1000.0, 700.0]);
    harness.run();
    harness.click_label("Add project");
    harness.run();
    // When: the operator tries Browse.
    harness.click_label("Browse…");
    harness.run();
    // Then: the error is visible without adding a project.
    assert!(harness.has_label("portal unavailable"));
    assert!(harness.state().sidebar().projects.is_empty());
}

#[test]
fn settings_modal_reveals_root_and_renames_without_changing_id() {
    // Given: a registered, persisted project whose root is not printed in the sidebar.
    let temp = tempfile::tempdir().expect("temp dir");
    let save = temp.path().join("sidebar.json");
    let sidebar = sidebar_with_project(temp.path());
    let root = sidebar.projects[0].repo_root.display().to_string();
    let mut harness =
        typing_harness(state(MockSource::default(), sidebar).with_sidebar_path(save.clone()));
    harness.run_steps(4);
    assert!(harness.query_by_label(&root).is_none());
    // When: the operator opens its settings and renames it.
    click(&mut harness, "Project settings");
    assert!(harness.query_by_label(&root).is_some());
    type_into(&mut harness, NAME_LABEL, "-fork");
    click(&mut harness, "Rename");
    // Then: only the display name changes, and the change is persisted.
    let project = &harness.state().sidebar().projects[0];
    assert_eq!(project.id, ProjectId::new("demo"));
    assert_eq!(project.name, "demo-fork");
    assert_eq!(
        workspace_ui::load_sidebar(&save)
            .expect("saved sidebar")
            .projects[0]
            .name,
        "demo-fork"
    );
}
