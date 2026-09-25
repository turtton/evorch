use gui::app::WorkbenchState;
use gui::fixture::DemoSource;
use gui::model::composer::{ImageAttachment, ProviderStatus};
use workspace_ui::{ProjectId, SidebarState, ThreadId, UiSettings};

fn state(root: &std::path::Path, path: &std::path::Path) -> WorkbenchState<DemoSource> {
    let mut sidebar = SidebarState::default();
    let project = ProjectId::new("demo");
    sidebar.add_project(project.clone(), "demo", root).unwrap();
    sidebar.select_project(&project).unwrap();
    for id in ["alpha", "beta"] {
        sidebar
            .create_thread(ThreadId::new(id), project.clone(), id)
            .unwrap();
    }
    sidebar.switch_thread(&ThreadId::new("alpha")).unwrap();
    WorkbenchState::new(DemoSource(Vec::new()), &UiSettings::default())
        .unwrap()
        .with_sidebar(sidebar)
        .with_sidebar_path(path.to_owned())
        .with_provider_status(ProviderStatus::Configured)
}

#[test]
fn switching_threads_restores_text_and_attachments_and_persists_text() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("sidebar.json");
    let mut state = state(temp.path(), &path);
    state.composer_mut().input = "unfinished alpha".into();
    state.composer_mut().attachments.push(ImageAttachment {
        media_type: "image/png".into(),
        data: "aGVsbG8=".into(),
    });

    state.switch_thread(ThreadId::new("beta")).unwrap();
    assert!(state.composer().input.is_empty());
    assert!(state.composer().attachments.is_empty());
    state.composer_mut().input = "unfinished beta".into();
    state.switch_thread(ThreadId::new("alpha")).unwrap();
    assert_eq!(state.composer().input, "unfinished alpha");
    assert_eq!(state.composer().attachments.len(), 1);
    state.switch_thread(ThreadId::new("beta")).unwrap();
    assert_eq!(state.composer().input, "unfinished beta");
    assert!(state.composer().attachments.is_empty());

    let saved = workspace_ui::load_sidebar(&path).unwrap();
    assert_eq!(saved.threads[0].draft_input, "unfinished alpha");
    assert_eq!(saved.threads[1].draft_input, "unfinished beta");
    let restored = WorkbenchState::new(DemoSource(Vec::new()), &UiSettings::default())
        .unwrap()
        .with_sidebar(saved);
    assert_eq!(restored.composer().input, "unfinished beta");
}

#[test]
fn sending_clears_only_the_active_thread_draft() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("sidebar.json");
    let mut state = state(temp.path(), &path);
    state.composer_mut().input = "keep alpha".into();
    state.switch_thread(ThreadId::new("beta")).unwrap();
    state.composer_mut().input = "send beta".into();
    state.submit_composer();
    assert!(state.composer().input.is_empty());
    state.switch_thread(ThreadId::new("alpha")).unwrap();
    assert_eq!(state.composer().input, "keep alpha");
    state.switch_thread(ThreadId::new("beta")).unwrap();
    assert!(state.composer().input.is_empty());
    let saved = workspace_ui::load_sidebar(&path).unwrap();
    assert!(saved.threads[1].draft_input.is_empty());
}

#[test]
fn new_thread_command_is_not_saved_as_the_previous_draft() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("sidebar.json");
    let mut state = state(temp.path(), &path);
    state.composer_mut().input = "/new".into();
    state.submit_composer();
    state.switch_thread(ThreadId::new("alpha")).unwrap();
    assert!(state.composer().input.is_empty());
}

#[test]
fn active_draft_is_saved_after_a_frame_without_switching() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("sidebar.json");
    let mut harness =
        gui::headless::HeadlessWorkbench::new(state(temp.path(), &path), [900.0, 700.0]);
    harness.state_mut().composer_mut().input = "still typing".into();
    harness.run();
    let saved = workspace_ui::load_sidebar(&path).unwrap();
    assert_eq!(saved.threads[0].draft_input, "still typing");
}
