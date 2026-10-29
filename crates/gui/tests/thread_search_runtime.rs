//! The search modal against the real runtime and run store: the cursor chain a
//! project's ranked matches need must reach a match the sidebar can open even
//! when every earlier page holds only threads it cannot.

use std::sync::Arc;

use async_trait::async_trait;
use event_bus::EventBus;
use gui::app::WorkbenchState;
use gui::headless::HeadlessWorkbench;
use gui::model::commands::{CommandSink, LoopEvent, WorkbenchCommand};
use gui::model::thread_search::ThreadSearchReceiver;
use gui::panes::thread_search::NO_MATCHES_LABEL;
use providers::{ChatResponse, Message, ToolSpec};
use runtime::{AgentInvocationContext, AgentModel, AgentRuntime, Role, RunStore, RuntimeError};
use storage::threads::ThreadUpsert;
use tools::ToolExecutor;
use workspace_ui::{ProjectId, SidebarState, ThreadId, ThreadRecord, UiSettings};

struct IdleModel;

#[async_trait]
impl AgentModel for IdleModel {
    async fn complete(
        &self,
        _invocation: &AgentInvocationContext,
        _role: Role,
        _messages: &[Message],
        _tools: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        std::future::pending().await
    }

    fn selected_model(&self, role: Role, _category: Option<&str>) -> String {
        format!("test-{}", role.name().to_lowercase())
    }
}

/// Answers each search on the calling thread with the real runtime, so the
/// whole cursor chain is deterministic and needs no waiting.
struct RuntimeSearchSink(AgentRuntime);

impl CommandSink for RuntimeSearchSink {
    fn submit(&mut self, _: WorkbenchCommand) -> Vec<LoopEvent> {
        Vec::new()
    }

    fn search_threads(
        &self,
        project: std::path::PathBuf,
        query: String,
        limit: usize,
        cursor: Option<String>,
    ) -> Option<ThreadSearchReceiver> {
        let (sender, receiver) = std::sync::mpsc::channel();
        let found = self
            .0
            .search_threads_for_host(&project, &query, limit, cursor.as_deref());
        sender.send(found).ok()?;
        Some(receiver)
    }
}

#[test]
fn search_reaches_the_one_openable_match_behind_a_page_of_unopenable_ones() {
    // Given: 21 threads whose titles match "pg" only as scattered letters, in the
    // run store; the sidebar lists just the lowest-ranked one.
    let project_dir = tempfile::tempdir().unwrap();
    let root = project_dir.path().canonicalize().unwrap();
    let db = tempfile::tempdir().unwrap();
    let config = storage::StorageConfig {
        db_path: db.path().join("runs.db"),
        ..Default::default()
    };
    let storage = storage::Storage::open(config.clone()).unwrap();
    let bus = Arc::new(EventBus::new(256));
    let runtime = AgentRuntime::new(
        Arc::clone(&bus),
        Arc::new(ToolExecutor::new(bus)),
        Arc::new(IdleModel),
    )
    .with_run_store(RunStore::open(&config, storage.handle()).unwrap());
    for index in 0..21_i64 {
        storage
            .handle()
            .upsert_thread_runtime(&ThreadUpsert {
                thread_id: format!("t{index:02}"),
                project_root: Some(root.to_string_lossy().into_owned()),
                root_run_id: Some(format!("run-{}", 100 + index)),
                title: Some(format!("Page group {index}")),
                created_at_ns: 1,
                updated_at_ns: 100 + index,
                ..Default::default()
            })
            .unwrap();
    }
    // The oldest thread ranks last among equal titles.
    let mut sidebar = SidebarState::default();
    let project = ProjectId::new("project");
    sidebar
        .add_project(project.clone(), "Project", &root)
        .unwrap();
    sidebar.select_project(&project).unwrap();
    sidebar.threads.push(ThreadRecord::new(
        ThreadId::new("t00"),
        project,
        "Page group 0",
    ));
    let state = WorkbenchState::new(runtime.clone(), &UiSettings::default())
        .unwrap()
        .with_sidebar(sidebar)
        .with_command_sink(Box::new(RuntimeSearchSink(runtime)));
    let mut gui = HeadlessWorkbench::new(state, [1200.0, 800.0]);
    gui.run();

    // When: searching "pg" through the modal.
    gui.state_mut().open_thread_search();
    gui.run();
    gui.input_mut().events.push(egui::Event::Text("pg".into()));
    gui.run();
    gui.run();

    // Then: the cursor chain passes the 20 unopenable matches and lists the 21st,
    // without ever claiming there is no match.
    let rows: Vec<_> = gui
        .state()
        .thread_search()
        .rows(gui.state().sidebar())
        .iter()
        .map(|row| row.thread.to_string())
        .collect();
    assert_eq!(rows, ["t00"]);
    assert!(!gui.has_label(NO_MATCHES_LABEL));
}
