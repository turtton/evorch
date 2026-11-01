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
        evidence_id: None,
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
    let mut app = Ready {
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
        executor: None,
        _storage: GuiStorageResources {
            _storage: bridge,
            ownership,
            _demo_directory: None,
            _storage_fallback: None,
            _leases: storage::shared_with_wait(
                &config.db_path,
                &directory.path().join("lease-root"),
                Duration::from_secs(1),
            )
            .unwrap(),
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

#[test]
fn dropping_the_running_app_stops_the_executor_before_the_locks_are_released() {
    // Given: a running app with a reclaim executor holding the reclaim lease, and
    // shared start-up locks in its storage resources.
    let directory = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: directory.path().join("events.db"),
        ..Default::default()
    };
    let root = directory.path().join("owners");
    let leases = storage::shared_with_wait(&config.db_path, &root, Duration::from_secs(1)).unwrap();
    let storage = Storage::open_with_lease(config.clone(), leases.db_lease().unwrap()).unwrap();
    let bus = Arc::new(EventBus::new(32));
    let runtime = AgentRuntime::new(
        bus.clone(),
        Arc::new(tools::ToolExecutor::new(bus.clone())),
        Arc::new(DemoScriptModel::new(bus.clone())),
    );
    let state = WorkbenchState::new(runtime, &UiSettings::default()).unwrap();
    let ownership = Arc::new(
        runtime::ownership::OwnerHost::open(&root, Default::default(), bus.clone()).unwrap(),
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
    let executor =
        ReclaimExecutor::spawn(ExecutorConfig::new(bridge.handle(), config.clone(), None)).unwrap();
    // The executor takes its lease on the first cycle.
    assert!(executor.wait_for_cycles(1).holds_lease);
    let app = Ready {
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
        executor: Some(executor),
        _storage: GuiStorageResources {
            _storage: bridge,
            ownership,
            _demo_directory: None,
            _storage_fallback: None,
            _leases: leases,
        },
    };
    assert!(storage::reclaim_lease(&config.db_path).unwrap().is_none());
    assert!(
        storage::try_exclusive(&config.db_path, &root)
            .unwrap()
            .is_none()
    );

    // When: the app is dropped.
    drop(app);

    // Then: the reclaim lease and the start-up locks are all released.
    assert!(storage::reclaim_lease(&config.db_path).unwrap().is_some());
    assert!(
        storage::try_exclusive(&config.db_path, &root)
            .unwrap()
            .is_some()
    );
}
