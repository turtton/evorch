use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use gui::app::WorkbenchState;
use gui::fixture::DemoSource;
use gui::headless::HeadlessWorkbench;
use gui::model::commands::{CommandSink, LoopEvent, WorkbenchCommand};
use gui::model::composer::ProviderStatus;
use workspace_ui::{ProjectId, SidebarState, ThreadId, UiSettings};

type Recorded = Arc<Mutex<Vec<Option<PathBuf>>>>;

struct RecordingSink {
    cwd: Recorded,
    chats: Recorded,
}

impl CommandSink for RecordingSink {
    fn set_default_cwd(&mut self, cwd: Option<PathBuf>) -> Result<(), String> {
        self.cwd.lock().expect("recorded directories").push(cwd);
        Ok(())
    }

    fn submit(&mut self, command: WorkbenchCommand) -> Vec<LoopEvent> {
        if let WorkbenchCommand::SendChat(chat) = command {
            self.chats
                .lock()
                .expect("recorded chats")
                .push(chat.project_root);
        }
        Vec::new()
    }
}

#[test]
fn chats_run_in_the_project_of_the_thread_they_are_sent_from() {
    // Given: one thread in each of two projects, with the first project selected.
    let root = tempfile::tempdir().expect("fixture");
    let mut sidebar = SidebarState::default();
    for name in ["first", "second"] {
        let path = root.path().join(name);
        std::fs::create_dir(&path).expect("directory");
        sidebar
            .add_project(ProjectId::new(name), name, &path)
            .expect("project");
        sidebar
            .create_thread(ThreadId::new(name), ProjectId::new(name), name)
            .expect("thread");
    }
    sidebar
        .select_project(&ProjectId::new("first"))
        .expect("selection");
    let (cwd, chats) = (Recorded::default(), Recorded::default());
    let state = WorkbenchState::new(DemoSource(Vec::new()), &UiSettings::default())
        .expect("workbench")
        .with_sidebar(sidebar)
        .with_command_sink(Box::new(RecordingSink {
            cwd: cwd.clone(),
            chats: chats.clone(),
        }))
        .with_provider_status(ProviderStatus::Configured);
    let mut harness = HeadlessWorkbench::new(state, [1200.0, 900.0]);

    // When: a chat is sent from each thread in turn.
    for thread in ["second", "first"] {
        harness
            .state_mut()
            .switch_thread(ThreadId::new(thread))
            .expect("switch");
        harness.state_mut().composer_mut().input = format!("from {thread}");
        harness.state_mut().submit_composer();
    }

    // Then: each chat and the shell directory follow the sending thread's project.
    let expected = vec![
        Some(root.path().join("second")),
        Some(root.path().join("first")),
    ];
    assert_eq!(*chats.lock().expect("chats"), expected);
    // Switching threads moves the shell too, before anything is sent.
    let expected_cwd = vec![
        Some(root.path().join("second")),
        Some(root.path().join("second")),
        Some(root.path().join("first")),
        Some(root.path().join("first")),
    ];
    assert_eq!(*cwd.lock().expect("directories"), expected_cwd);
}
