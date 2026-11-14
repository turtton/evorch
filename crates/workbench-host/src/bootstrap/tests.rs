use super::*;

#[test]
fn project_directory_preserves_sidebar_ids_without_writing_sidebar_state() {
    let dir = test_socket_dir::tempdir();
    let root = dir.path().canonicalize().unwrap();
    let sidebar = serde_json::json!({
        "projects": [{"id":"desktop-project", "repo_root":root, "name":"Project"}],
        "threads": [{"draft":"must not change"}]
    })
    .to_string();
    std::fs::write(root.join("sidebar.json"), &sidebar).unwrap();
    let options = BootstrapOptions {
        config_dir: root.clone(),
        project_root: root.clone(),
        ownership_root: root.join("owners"),
    };
    let projects = known_projects(&options).unwrap().directory.entries();
    assert_eq!(projects.len(), 1);
    assert_eq!(projects[0].0.as_str(), "desktop-project");
    assert_eq!(projects[0].1, root);
    assert_eq!(
        std::fs::read_to_string(options.config_dir.join("sidebar.json")).unwrap(),
        sidebar
    );
}

#[tokio::test]
async fn bootstrap_drains_before_releasing_its_owner() {
    let dir = test_socket_dir::tempdir();
    let root = dir.path().canonicalize().unwrap();
    let mut boot = BootstrappedHost::open(BootstrapOptions {
        config_dir: root.clone(),
        project_root: root.clone(),
        ownership_root: root.join("owners"),
    })
    .await
    .unwrap();
    let _permit = boot.owner.start("owned").unwrap();
    let owner = boot.owner.clone();
    let (stopped, completed) = tokio::sync::oneshot::channel();
    boot.services.start("test-service", |stop| async move {
        stop.await?;
        assert_eq!(
            owner.attach("owned")?.state,
            runtime::ownership::OwnerState::Quiescing
        );
        stopped.send(()).unwrap();
        Ok(())
    });
    assert!(!boot.busy());
    boot.shutdown().await.unwrap();
    completed.await.unwrap();
    assert_eq!(
        boot.owner.attach("owned").unwrap().state,
        runtime::ownership::OwnerState::Released
    );
}

#[tokio::test]
async fn saved_access_only_declares_approved_existing_directories_without_rewriting_state() {
    let dir = test_socket_dir::tempdir();
    let root = dir.path().canonicalize().unwrap();
    let allowed = root.join("allowed");
    let rejected = root.join("unapproved");
    std::fs::create_dir(&allowed).unwrap();
    std::fs::create_dir(&rejected).unwrap();
    let mut document = workspace_state::SidebarDocument::default();
    document
        .sidebar
        .projects
        .push(workspace_state::ProjectRecord {
            id: workspace_state::ProjectId::new("desktop"),
            name: "Project".into(),
            repo_root: root.clone(),
            trust: workspace_state::TrustState::Approved,
            allowed_directories: [
                (allowed.clone(), workspace_state::TrustState::Approved),
                (allowed.clone(), workspace_state::TrustState::Approved),
                (rejected, workspace_state::TrustState::Unapproved),
                (root.join("missing"), workspace_state::TrustState::Approved),
            ]
            .into_iter()
            .map(|(path, trust)| workspace_state::AllowedDirectory { path, trust })
            .collect(),
        });
    document
        .sidebar
        .threads
        .push(workspace_state::ThreadRecord {
            draft_input: "untouched".into(),
            ..workspace_state::ThreadRecord::new(
                workspace_state::ThreadId::new("t1"),
                workspace_state::ProjectId::new("desktop"),
                "Conversation",
            )
        });
    let sidebar = serde_json::to_string(&document).unwrap();
    std::fs::write(root.join("sidebar.json"), &sidebar).unwrap();
    let mut boot = BootstrappedHost::open(BootstrapOptions {
        config_dir: root.clone(),
        project_root: root.clone(),
        ownership_root: root.join("owners"),
    })
    .await
    .unwrap();
    assert_eq!(
        boot.runtime
            .project_allowed_directories(&root)
            .get()
            .as_ref(),
        &[allowed]
    );
    boot.shutdown().await.unwrap();
    assert_eq!(
        std::fs::read_to_string(root.join("sidebar.json")).unwrap(),
        sidebar
    );
}

#[test]
fn absent_access_is_unapproved_and_unknown_trust_does_not_grant_access() {
    let access: components::ProjectAccess = serde_json::from_str("{}").unwrap();
    assert!(matches!(access.trust, components::Trust::Unapproved));
    assert!(access.allowed_directories.is_empty());
    assert!(serde_json::from_str::<components::ProjectAccess>(r#"{"trust":"trusted"}"#).is_err());
}

#[tokio::test]
async fn core_catalog_prices_usage_and_diagnostics_are_flushed_once() {
    let dir = test_socket_dir::tempdir();
    let root = dir.path().canonicalize().unwrap();
    let catalog = root.join("catalog");
    std::fs::create_dir(&catalog).unwrap();
    // Timestamp is fixture input for catalog freshness, not a completion deadline.
    let fetched_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    std::fs::write(
        catalog.join("models-dev.json"),
        serde_json::json!({
            "fetched_at": fetched_at,
            "api":{"openai":{"models":{"gpt-test":{
                "id":"gpt-test","cost":{"input":2.0,"output":4.0}
            }}}}
        })
        .to_string(),
    )
    .unwrap();
    let mut boot = BootstrappedHost::open(BootstrapOptions {
        config_dir: root.clone(),
        project_root: root.clone(),
        ownership_root: root.join("owners"),
    })
    .await
    .unwrap();
    // This current-thread test subscribes before yielding to the service factory.
    let bus = boot.runtime.event_bus().clone();
    let mut receiver = bus.subscribe();
    loop {
        let event = receiver.recv().await.unwrap();
        if let event_bus::EventKind::Diagnostic(diagnostic) = event.kind
            && diagnostic.source == "catalog"
        {
            assert_eq!(diagnostic.code, "catalog.loaded");
            break;
        }
    }
    assert_eq!(
        boot.usage_pricing
            .read()
            .unwrap()
            .pricing("openai", None, "gpt-test")
            .unwrap()
            .standard
            .base
            .input,
        Some(2.0)
    );
    bus.emit(event_bus::Event::new(event_bus::DiagnosticEvent {
        source: "core-test".into(),
        severity: event_bus::DiagnosticSeverity::Warning,
        code: "core.test".into(),
        detail: "Persist this diagnostic once".into(),
        run_id: None,
        thread_id: None,
        call_id: None,
    }));
    bus.emit(event_bus::Event::new(
        event_bus::ProviderEvent::RequestCompleted {
            request_id: "core-priced".into(),
            provider: "openai".into(),
            profile: None,
            protocol: "openai-chat-completions".into(),
            model: "gpt-test".into(),
            usage_reported: true,
            streaming: true,
            duration_ms: 1,
            input_tokens: 100,
            output_tokens: 20,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            reasoning_tokens: None,
            finish_reason: "stop".into(),
            service_tier: None,
            run_id: None,
            purpose: None,
        },
    ));
    boot.shutdown().await.unwrap();
    let db = storage::Database::open(&boot.composition.storage).unwrap();
    let events = db.events_all_ordered().unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(
                &event.event.kind,
                event_bus::EventKind::Diagnostic(diagnostic) if diagnostic.code == "core.test"
            ))
            .count(),
        1
    );
    let usage = db.usage_requests_between(0, i64::MAX).unwrap();
    assert_eq!(usage.len(), 1);
    assert_eq!(usage[0].record.request_id, "core-priced");
    assert!((usage[0].record.cost_usd.unwrap() - 0.00028).abs() < f64::EPSILON);
}

#[tokio::test]
async fn restart_quiesce_keeps_owner_alive_until_explicit_operator_shutdown() {
    let dir = test_socket_dir::tempdir();
    let root = dir.path().canonicalize().unwrap();
    let mut boot = BootstrappedHost::open(BootstrapOptions {
        config_dir: root.clone(),
        project_root: root.clone(),
        ownership_root: root.join("owners"),
    })
    .await
    .unwrap();
    let permit = boot.owner.start("restart").unwrap();
    boot.quiesce_for_restart().await.unwrap();
    assert_eq!(
        boot.runtime.quiesce_state(),
        runtime::quiesce::QuiesceState::Checkpointed
    );
    assert_ne!(
        boot.owner.attach("restart").unwrap().state,
        runtime::ownership::OwnerState::Released
    );
    permit.validate_generation().unwrap();
    boot.shutdown().await.unwrap();
    assert_eq!(
        boot.owner.attach("restart").unwrap().state,
        runtime::ownership::OwnerState::Released
    );
}

#[tokio::test]
async fn area_tasks_are_stopped_and_drained_by_the_bootstrap_shutdown() {
    let dir = test_socket_dir::tempdir();
    let root = dir.path().canonicalize().unwrap();
    let mut boot = BootstrappedHost::open(BootstrapOptions {
        config_dir: root.clone(),
        project_root: root.clone(),
        ownership_root: root.join("owners"),
    })
    .await
    .unwrap();
    // The areas start event tasks of their own; the probe is one more.
    let areas_running = boot.area_tasks.active();
    let (drained, completed) = tokio::sync::oneshot::channel();
    boot.area_tasks
        .spawn("probe", |mut stop| async move {
            stop.stopped().await;
            drained.send(()).unwrap();
            Ok(())
        })
        .unwrap();
    assert_eq!(boot.area_tasks.active(), areas_running + 1);
    boot.shutdown().await.unwrap();
    // The task ran its drain before shutdown returned, and nothing new starts.
    completed.await.unwrap();
    assert_eq!(boot.area_tasks.active(), 0);
    assert_eq!(
        boot.area_tasks
            .spawn("late", |_| async { Ok(()) })
            .unwrap_err(),
        crate::areas::SpawnError::Closed
    );
}

#[tokio::test]
async fn a_failing_area_task_is_reported_but_the_drain_flush_and_owner_release_still_happen() {
    let dir = test_socket_dir::tempdir();
    let root = dir.path().canonicalize().unwrap();
    let mut boot = BootstrappedHost::open(BootstrapOptions {
        config_dir: root.clone(),
        project_root: root.clone(),
        ownership_root: root.join("owners"),
    })
    .await
    .unwrap();
    let _permit = boot.owner.start("owned").unwrap();
    let (drained, completed) = tokio::sync::oneshot::channel();
    boot.area_tasks
        .spawn("failing", |mut stop| async move {
            stop.stopped().await;
            Err("area failed".into())
        })
        .unwrap();
    boot.area_tasks
        .spawn("healthy", |mut stop| async move {
            stop.stopped().await;
            drained.send(()).unwrap();
            Ok(())
        })
        .unwrap();
    // An event the bridge still has to flush when the shutdown runs.
    boot.runtime
        .event_bus()
        .emit(event_bus::Event::new(event_bus::DiagnosticEvent {
            source: "area-test".into(),
            severity: event_bus::DiagnosticSeverity::Warning,
            code: "area.failure.flush".into(),
            detail: "Persisted although an area task failed".into(),
            run_id: None,
            thread_id: None,
            call_id: None,
        }));

    let error = boot.shutdown().await.unwrap_err().to_string();
    assert_eq!(error, "failing: area failed");
    // The other task was joined, the bridge flushed, and the owner released.
    completed.await.unwrap();
    let db = storage::Database::open(&boot.composition.storage).unwrap();
    assert_eq!(
        db.events_all_ordered()
            .unwrap()
            .iter()
            .filter(|event| matches!(
                &event.event.kind,
                event_bus::EventKind::Diagnostic(d) if d.code == "area.failure.flush"
            ))
            .count(),
        1
    );
    assert_eq!(
        boot.owner.attach("owned").unwrap().state,
        runtime::ownership::OwnerState::Released
    );
}
