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
    let directory = test_socket_dir::SplitDir::new();
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
        provenance: storage::HostInputProvenance::User,
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
            &directory.sockets().join("owner"),
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
            background: None,
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
    let directory = test_socket_dir::SplitDir::new();
    let config = StorageConfig {
        db_path: directory.path().join("events.db"),
        ..Default::default()
    };
    let root = directory.sockets().to_path_buf();
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
            background: None,
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

#[test]
fn the_start_up_wiring_makes_both_the_runtime_and_the_host_refuse_deleted_threads() {
    // Given: a runtime and an ownership host as the start-up composes them, and the
    // tombstones it read after the locks became shared.
    let directory = test_socket_dir::SocketDir::new();
    let bus = Arc::new(EventBus::new(32));
    let runtime = AgentRuntime::new(
        bus.clone(),
        Arc::new(tools::ToolExecutor::new(bus.clone())),
        Arc::new(DemoScriptModel::new(bus.clone())),
    );
    let host =
        runtime::ownership::OwnerHost::open(directory.path(), Default::default(), bus).unwrap();
    let tombstones = storage::thread_retention::TombstoneSet {
        threads: ["gone".to_owned()].into(),
        runs: ["gone-run".to_owned()].into(),
        ..Default::default()
    };
    let goal = |thread: &str, root: &str| -> event_bus::ThreadGoalSnapshot {
        serde_json::from_value(serde_json::json!({
            "goal_id": "g", "thread_id": thread, "root_run_id": root,
            "related_root_run_ids": [], "objective": "o", "criteria": [], "checks": [],
            "phase": "working", "review_enabled": false, "checks_paused": false,
            "work_stopped": false, "epoch": 1, "review_round": 0, "findings": [],
            "reason": null,
            "usage": {"model_requests": 0, "input_tokens": 0, "output_tokens": 0},
            "max_review_rounds": 1, "max_tokens": null, "original_request": "r"
        }))
        .unwrap()
    };

    // When: the wiring runs.
    let guarded = install_runtime_tombstones(&runtime, &tombstones);
    install_host_tombstones(&host, &tombstones);

    // Then: a restore of a deleted thread or a deleted run is refused, a live one is
    // not, and the host refuses to start the deleted thread.
    let refusal = |thread, root| {
        runtime
            .restore_thread_goal(goal(thread, root))
            .err()
            .unwrap_or_default()
    };
    assert!(refusal("gone", "r1").contains("削除"));
    assert!(refusal("live", "gone-run").contains("削除"));
    assert!(!refusal("live", "r2").contains("削除"));
    assert!(host.start("gone").is_err());
    assert!(host.start("live").is_ok());
    // And: the restore helpers only accept the proof the wiring returns.
    restore_thread_goals(Default::default(), &runtime, &guarded);
}
