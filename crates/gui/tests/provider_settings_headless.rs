use gui::app::WorkbenchState;
use gui::fixture::DemoSource;
use gui::headless::HeadlessWorkbench;
use gui::model::composer::{PROVIDER_MISSING_GUIDANCE, ProviderStatus};
use workspace_ui::{ProjectId, SidebarState, ThreadId, UiSettings};

fn workbench(root: &std::path::Path, provider: ProviderStatus) -> HeadlessWorkbench<DemoSource> {
    let mut sidebar = SidebarState::default();
    let project_id = ProjectId::new("demo");
    sidebar
        .add_project(project_id.clone(), "demo", root)
        .expect("project added");
    sidebar
        .select_project(&project_id)
        .expect("project selected");
    sidebar
        .create_thread(ThreadId::new("thread-1"), project_id, "thread-1")
        .expect("thread created");
    sidebar
        .switch_thread(&ThreadId::new("thread-1"))
        .expect("thread selected");
    let state = WorkbenchState::new(DemoSource(Vec::new()), &UiSettings::default())
        .expect("default state builds")
        .with_sidebar(sidebar)
        .with_provider_status(provider);
    HeadlessWorkbench::new(state, [1200.0, 900.0])
}

#[test]
fn open_settings_button_visible_when_not_configured() {
    // Given: conversations with and without a configured provider.
    let temp = tempfile::tempdir().expect("temp dir");
    let mut missing = workbench(temp.path(), ProviderStatus::default());
    let mut configured = workbench(temp.path(), ProviderStatus::Configured);
    // When: both conversations render.
    missing.run();
    configured.run();
    // Then: only the unconfigured conversation offers settings.
    assert!(missing.has_label(PROVIDER_MISSING_GUIDANCE));
    assert!(missing.has_label("Open Settings"));
    assert!(!configured.has_label("Open Settings"));
}

#[test]
fn clicking_open_settings_shows_modal_fields() {
    // Given: an unconfigured conversation.
    let temp = tempfile::tempdir().expect("temp dir");
    let mut harness = workbench(temp.path(), ProviderStatus::default());
    harness.run();
    // When: settings are opened through the composer.
    harness.click_label("Open Settings");
    harness.run();
    // Then: the settings modal offers save and cancel.
    assert!(harness.has_label("Provider settings"));
    assert!(harness.has_label("Save"));
    assert!(harness.has_label("Cancel"));
}
