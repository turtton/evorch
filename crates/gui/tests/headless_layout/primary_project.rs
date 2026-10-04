use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use gui::app::WorkbenchState;
use gui::fixture::DemoSource;
use gui::headless::HeadlessWorkbench;
use gui::model::commands::{CommandSink, LoopEvent, WorkbenchCommand};
use gui::model::composer::ProviderStatus;
use workspace_ui::{ProjectId, SidebarState, ThreadId, UiSettings};

struct CwdSink(Arc<Mutex<Vec<Option<PathBuf>>>>);

impl CommandSink for CwdSink {
    fn set_default_cwd(&mut self, cwd: Option<PathBuf>) -> Result<(), String> {
        self.0.lock().expect("recorded directories").push(cwd);
        Ok(())
    }

    fn submit(&mut self, _: WorkbenchCommand) -> Vec<LoopEvent> {
        Vec::new()
    }
}

#[test]
fn star_toggle_changes_the_directory_sent_with_next_chat() {
    // Given: a selected second project, with the first project marked primary.
    let root = tempfile::tempdir().expect("fixture");
    let mut sidebar = SidebarState::default();
    for name in ["first", "second"] {
        let path = root.path().join(name);
        std::fs::create_dir(&path).expect("directory");
        sidebar
            .add_project(ProjectId::new(name), name, &path)
            .expect("project");
    }
    sidebar
        .select_project(&ProjectId::new("second"))
        .expect("selection");
    sidebar
        .set_primary_project(Some(ProjectId::new("first")))
        .expect("primary");
    sidebar
        .create_thread(ThreadId::new("chat"), ProjectId::new("second"), "chat")
        .expect("thread");
    sidebar
        .switch_thread(&ThreadId::new("chat"))
        .expect("active thread");
    let calls = Arc::new(Mutex::new(Vec::new()));
    let state = WorkbenchState::new(DemoSource(Vec::new()), &UiSettings::default())
        .expect("workbench")
        .with_sidebar(sidebar)
        .with_command_sink(Box::new(CwdSink(calls.clone())))
        .with_provider_status(ProviderStatus::Configured);
    let mut harness = HeadlessWorkbench::new(state, [1200.0, 900.0]);
    harness.state_mut().composer_mut().input = "before".into();
    harness.state_mut().submit_composer();
    assert_eq!(
        *calls.lock().expect("calls"),
        vec![Some(root.path().join("first"))]
    );
    harness.run();
    // When: the user clears the primary star and submits another chat.
    harness.click_label("Clear primary project");
    harness.run();
    harness.state_mut().composer_mut().input = "after".into();
    harness.state_mut().submit_composer();
    // Then: primary is cleared and the selected project becomes the shell directory.
    assert_eq!(
        *calls.lock().expect("calls"),
        vec![
            Some(root.path().join("first")),
            Some(root.path().join("second"))
        ]
    );
    assert!(!harness.has_label("Clear primary project"));
}
