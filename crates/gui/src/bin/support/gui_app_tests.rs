//! Exercise the production outer eframe App without ever painting a window.
use super::*;
use gui::model::commands::{CommandSink, LoopEvent, WorkbenchCommand};
use std::sync::Mutex;

struct Receipts(Arc<Mutex<Vec<storage::HostInputReceipt>>>);
impl CommandSink for Receipts {
    fn has_host_input_receipts(&self) -> bool {
        true
    }
    fn host_input_receipts(
        &self,
        _: &[String],
    ) -> Option<Result<Vec<storage::HostInputReceipt>, storage::StorageError>> {
        Some(Ok(self.0.lock().unwrap().clone()))
    }
    fn submit(&mut self, _: WorkbenchCommand) -> Vec<LoopEvent> {
        vec![]
    }
}

#[test]
fn outer_app_logic_only_saves_debounced_draft_and_repairs_missing_receipts() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sidebar.json");
    let config = StorageConfig {
        db_path: directory.path().join("events.db"),
        ..Default::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let bus = Arc::new(EventBus::new(32));
    let runtime = AgentRuntime::new(
        bus.clone(),
        Arc::new(tools::ToolExecutor::new(bus.clone())),
        Arc::new(DemoScriptModel::new(bus.clone())),
    );
    let mut sidebar = SidebarState::default();
    let project = ProjectId::new("test");
    sidebar
        .add_project(project.clone(), "test", directory.path())
        .unwrap();
    sidebar.select_project(&project).unwrap();
    sidebar
        .create_thread(ThreadId::new("thread"), project, "thread")
        .unwrap();
    sidebar.switch_thread(&ThreadId::new("thread")).unwrap();
    let mut saved = serde_json::to_value(&sidebar).unwrap();
    saved["user_messages"] = serde_json::json!([{
        "id": "pending", "thread_id": "thread", "text": "repair me",
        "prompt": "repair me", "at": std::time::SystemTime::now(),
        "status": {"state": "unknown"}
    }]);
    std::fs::write(&path, serde_json::to_vec(&saved).unwrap()).unwrap();
    let receipts = Arc::new(Mutex::new(vec![storage::HostInputReceipt {
        prompt_id: "pending".into(),
        run_id: "run-1".into(),
        root_run_id: "run-1".into(),
        text: "repair me".into(),
        image_count: 0,
        status: storage::HostInputStatus::Unknown,
        reason: None,
        accepted_at_ns: 1,
        status_at_ns: 1,
    }]));
    let mut state = WorkbenchState::new(runtime, &UiSettings::default())
        .unwrap()
        .with_sidebar(sidebar)
        .with_sidebar_path(path.clone())
        .with_command_sink(Box::new(Receipts(receipts.clone())));
    state
        .restore_history(&Database::open(&config).unwrap())
        .unwrap();
    let ownership = Arc::new(
        runtime::ownership::OwnerHost::open(
            &directory.path().join("owner"),
            Default::default(),
            bus.clone(),
        )
        .unwrap(),
    );
    let bridge = spawn_storage_bridge(
        bus,
        storage,
        "test",
        config::DiagnosticPersistence::Off,
        false,
        SharedUsagePricing::default(),
        None,
    )
    .unwrap();
    let mut app = GuiApp {
        workbench: WorkbenchApp(state),
        #[cfg(feature = "cef")]
        html_previews: {
            let (_, requests) = std::sync::mpsc::channel();
            gui::html_preview::HtmlPreviewWindows::new(
                requests,
                Arc::new(gui::model::artifact_opener::PortalArtifactOpener),
                Arc::new(gui::model::html_snapshots::HtmlSnapshots::new(
                    directory.path().join("snapshots"),
                )),
            )
        },
        _storage: GuiStorageResources {
            _storage: bridge,
            ownership,
            _demo_directory: None,
            _storage_fallback: None,
        },
    };
    app.workbench.0.composer_mut().input = "hidden draft".into();
    let ctx = egui::Context::default();
    let tick = |app: &mut dyn eframe::App| {
        let _ = ctx.run_logic(&Default::default(), |ctx| {
            app.logic(ctx, &mut eframe::Frame::_new_kittest());
        });
    };
    tick(&mut app);
    assert_ne!(
        workspace_ui::load_sidebar(&path).unwrap().threads[0].draft_input,
        "hidden draft"
    );
    std::thread::sleep(Duration::from_millis(800));
    tick(&mut app);
    assert_eq!(
        workspace_ui::load_sidebar(&path).unwrap().threads[0].draft_input,
        "hidden draft"
    );
    // No EventPump or notification. Only the outer App's hidden logic ticks can repair it.
    receipts.lock().unwrap()[0].status = storage::HostInputStatus::Delivered;
    std::thread::sleep(Duration::from_millis(2250));
    tick(&mut app);
    assert!(app.workbench.0.transcripts().thread().entries().iter().any(|entry| {
        matches!(entry, gui::model::transcript::TranscriptEntry::UserMessage { text, .. } if text == "repair me")
    }));
}
