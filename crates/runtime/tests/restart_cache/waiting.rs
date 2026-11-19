use super::*;

#[tokio::test]
async fn tools_waiting_cancel_then_completion_keeps_wire_prefix_and_next_capture_valid() {
    for wire in [Wire::ChatCompletions, Wire::Responses, Wire::Codex] {
        let read = Arc::new(GatedRead {
            started: Notify::new(),
            release: Notify::new(),
            child_started: Notify::new(),
        });
        let mut h = harness_with_wire(
            wire,
            vec![
                read_response(2),
                text_response("parent ready"),
                read_response(0),
            ],
            1_000_000,
            read.clone(),
            Vec::new(),
            None,
            0.9,
            |_| {},
        );
        let config = storage::StorageConfig {
            db_path: h._directory.path().join("waiting.db"),
            ..Default::default()
        };
        let storage = storage::Storage::open(config.clone()).unwrap();
        h.runtime = h
            .runtime
            .with_run_store(runtime::RunStore::open(&config, storage.handle()).unwrap());
        let parent = h
            .runtime
            .delegate_chat(
                "restart-wait-thread",
                Role::Orchestrator,
                "keep the original wait future".into(),
                RunConfig {
                    interactive: true,
                    keep_alive: true,
                    ..Default::default()
                },
            )
            .unwrap();
        let mut events = through_phase(&mut h.receiver, parent, AgentRunPhase::Waiting).await;
        let child = h
            .runtime
            .delegate_background_as_child(parent, Role::Worker, "child", RunConfig::default())
            .unwrap();
        read.started.notified().await;
        // No parent request can occur until the explicit input below.
        loop {
            let event = h.receiver.recv().await.unwrap();
            let started = matches!(&event.kind, EventKind::Tool(event_bus::ToolEvent::ToolStarted { run_id: Some(id), .. }) if id == &child.to_string());
            if !matches!(event.kind, EventKind::Usage(_)) {
                events.push(event);
            }
            if started {
                break;
            }
        }
        h.mock.enqueue(vec![
            ScriptedResponse::tool_call(
                "wait-response",
                MODEL,
                0,
                "live-wait",
                "wait",
                [json!({"run_id": child.to_string()}).to_string()],
            ),
            text_response("child complete"),
            text_response("parent observed wait"),
            // The legacy wait yields status; normal post-batch notice delivery
            // requests one more response before the parent waits for input.
            text_response("after completion notice"),
        ]);
        h.runtime
            .send_message(parent, "wait for child".into())
            .unwrap();
        events.extend(through_phase(&mut h.receiver, parent, AgentRunPhase::Waiting).await);
        let mut changes = h.runtime.subscribe_restart_state();
        let attempt = h.runtime.begin_restart_quiesce().unwrap();
        loop {
            if h.runtime.resume_intents().unwrap().iter().any(|intent| {
                intent.run_id == parent
                    && intent.checkpoint.as_ref().is_some_and(|checkpoint| {
                        checkpoint.boundary == runtime::checkpoint::CheckpointBoundary::ToolsWaiting
                    })
            }) {
                break;
            }
            changes.changed().await.unwrap();
        }
        h.runtime.cancel_restart_quiesce(&attempt).unwrap();
        read.release.notify_one();
        h.runtime.wait(child).await.unwrap();
        // Until child's Done, only the child's provider can emit usage.
        events.extend(
            through_phase(&mut h.receiver, child, AgentRunPhase::Done)
                .await
                .into_iter()
                .filter(|event| !matches!(event.kind, EventKind::Usage(_))),
        );
        events.extend(through_phase(&mut h.receiver, parent, AgentRunPhase::Waiting).await);
        let mut requests = h
            .mock
            .recorded_requests()
            .into_iter()
            .filter(|request| request.path == wire.path())
            .map(|request| request.body)
            .collect::<Vec<_>>();
        assert_eq!(requests.len(), 7);
        requests.remove(4);
        requests.remove(2);
        assert_eq!(h.mock.remaining_scripts(), 0);
        verify_trace_of(&h, parent, &events, 0, &requests);

        // The completed wait must not leak into the next ordinary checkpoint.
        let recipes = std::collections::BTreeMap::from([(
            parent,
            runtime::quiesce::RecoveryRecipe {
                settings_reference: runtime::quiesce::CORE_RECOVERY_SETTINGS_REFERENCE.into(),
                policy: runtime::quiesce::ResumePolicy::CoreOwned,
                goal_id: None,
                team: None,
                lane: None,
                descriptor: None,
            },
        )]);
        h.runtime.begin_planned_restart_quiesce(&recipes).unwrap();
        h.runtime.wait_quiesced().await.unwrap();
        let fence = h.runtime.fence_dispatch_restart().await.unwrap();
        let gate = runtime::quiesce::HostRestartGate::default();
        assert!(gate.transition(1, runtime::quiesce::HostRestartPhase::Sealing));
        let sealed = gate.sealed(&h.runtime, 1, fence).await.unwrap();
        let tx = storage
            .handle()
            .mutate_restart(
                "waiting-capture".into(),
                None,
                storage::recovery::restart::RestartMutation::Create(Box::new(
                    support::fixture_restart_metadata("waiting-capture"),
                )),
            )
            .unwrap();
        let mut session = sealed.begin_capture(&h.runtime, &tx, recipes).unwrap();
        h.runtime
            .persist_restart_capture_expected(&mut session)
            .unwrap();
        let records = h.runtime.read_restart_capture_records(&session).unwrap();
        h.runtime.commit_restart_capture(&mut session).unwrap();
        let record = records
            .iter()
            .find(|record| record.run_id == parent)
            .unwrap();
        assert!(record.waits.is_empty());
        assert!(
            record
                .recipe
                .descriptor
                .as_ref()
                .unwrap()
                .recovery_waits
                .is_empty()
        );
        assert!(matches!(
            h.runtime.stop(parent, runtime::StopScope::SelfOnly),
            Err(runtime::RuntimeError::RestartBusy)
        ));
        h.runtime
            .fence_dispatch_shutdown(|| async { Ok::<_, String>(()) })
            .await
            .unwrap();
        h.runtime
            .stop(parent, runtime::StopScope::SelfOnly)
            .unwrap();
        h.runtime.wait(parent).await.unwrap();
    }
}

#[tokio::test]
async fn full_builtin_capture_orchestrator_and_child_worker_uses_one_registry_name_set() {
    capture_builtin_catalog(false).await;
}
#[tokio::test]
async fn full_builtin_capture_rejects_effective_tool_omitted_by_instance_only_fixture_override() {
    capture_builtin_catalog(true).await;
}
async fn capture_builtin_catalog(omit_visible: bool) {
    let read = Arc::new(GatedRead {
        started: Notify::new(),
        release: Notify::new(),
        child_started: Notify::new(),
    });
    let mut h = harness_with_wire(
        Wire::ChatCompletions,
        vec![text_response("parent ready"), text_response("child ready")],
        1_000_000,
        read,
        Vec::new(),
        None,
        0.9,
        |_| {},
    );
    let config = storage::StorageConfig {
        db_path: h._directory.path().join("full-catalog.db"),
        ..Default::default()
    };
    let storage = storage::Storage::open(config.clone()).unwrap();
    h.runtime = h
        .runtime
        .with_run_store(runtime::RunStore::open(&config, storage.handle()).unwrap());
    if omit_visible {
        let mut raw = h.runtime.restart_builtin_tool_catalog(&[]);
        assert!(raw.iter().any(|s| s.name == "wait"));
        raw.retain(|s| s.name != "wait");
        h.runtime
            .set_restart_builtin_catalog_for_fixture(raw.clone())
            .unwrap();
        assert!(
            h.runtime
                .set_restart_builtin_catalog_for_fixture(raw)
                .is_err()
        );
        assert!(!h.runtime.restart_builtin_tool_names().contains("wait"));
        let other = harness_with_wire(
            Wire::ChatCompletions,
            Vec::new(),
            1_000_000,
            Arc::new(GatedRead {
                started: Notify::new(),
                release: Notify::new(),
                child_started: Notify::new(),
            }),
            Vec::new(),
            None,
            0.9,
            |_| {},
        );
        assert!(
            other.runtime.restart_builtin_tool_names().contains("wait"),
            "fixture override leaked to another runtime"
        );
    }
    let parent = h
        .runtime
        .delegate_chat(
            "catalog-thread",
            Role::Orchestrator,
            "parent".into(),
            RunConfig {
                interactive: true,
                keep_alive: true,
                ..Default::default()
            },
        )
        .unwrap();
    through_phase(&mut h.receiver, parent, AgentRunPhase::Waiting).await;
    let child = h
        .runtime
        .delegate_background_as_child(
            parent,
            Role::Worker,
            "child",
            RunConfig {
                interactive: true,
                keep_alive: true,
                ..Default::default()
            },
        )
        .unwrap();
    through_phase(&mut h.receiver, child, AgentRunPhase::Waiting).await;
    let recipes = [parent, child]
        .into_iter()
        .map(|run| {
            (
                run,
                runtime::quiesce::RecoveryRecipe {
                    settings_reference: runtime::quiesce::CORE_RECOVERY_SETTINGS_REFERENCE.into(),
                    policy: runtime::quiesce::ResumePolicy::CoreOwned,
                    goal_id: None,
                    team: None,
                    lane: None,
                    descriptor: None,
                },
            )
        })
        .collect();
    h.runtime.begin_planned_restart_quiesce(&recipes).unwrap();
    h.runtime.wait_quiesced().await.unwrap();
    let fence = h.runtime.fence_dispatch_restart().await.unwrap();
    let gate = runtime::quiesce::HostRestartGate::default();
    assert!(gate.transition(1, runtime::quiesce::HostRestartPhase::Sealing));
    let sealed = gate.sealed(&h.runtime, 1, fence).await.unwrap();
    let mut metadata = support::fixture_restart_metadata("full-catalog");
    metadata.old_builtin_tool_names = h.runtime.restart_builtin_tool_names();
    let tx = storage
        .handle()
        .mutate_restart(
            "full-catalog".into(),
            None,
            storage::recovery::restart::RestartMutation::Create(Box::new(metadata.clone())),
        )
        .unwrap();
    let mut session = sealed.begin_capture(&h.runtime, &tx, recipes).unwrap();
    h.runtime
        .persist_restart_capture_expected(&mut session)
        .unwrap();
    let captured = h.runtime.persist_restart_tool_snapshots(&mut session);
    if omit_visible {
        assert!(
            captured
                .unwrap_err()
                .to_string()
                .contains("effective builtin missing from fixture build catalog")
        );
        let db = storage::Database::open(&config).unwrap();
        for member in &h.runtime.read_restart_capture_records(&session).unwrap() {
            assert!(
                db.restart_tool_snapshot(
                    "full-catalog",
                    &member.run_id.to_string(),
                    &member.old_incarnation
                )
                .unwrap()
                .is_none()
            );
        }
    } else {
        captured.unwrap();
        let committed = h.runtime.commit_restart_capture(&mut session).unwrap();
        assert_eq!(committed.expected.len(), 2);
        let db = storage::Database::open(&config).unwrap();
        for member in &committed.expected {
            let old = db
                .restart_tool_snapshot("full-catalog", &member.run_id, &member.incarnation)
                .unwrap()
                .unwrap();
            assert_eq!(
                old.builtins
                    .iter()
                    .map(|s| s["name"].as_str().unwrap().to_owned())
                    .collect::<std::collections::BTreeSet<_>>(),
                metadata.old_builtin_tool_names
            );
            assert!(
                old.effective.len() < old.builtins.len(),
                "role projection must not shrink the registry evidence"
            );
        }
    }
    h.runtime
        .fence_dispatch_shutdown(|| async { Ok::<_, String>(()) })
        .await
        .unwrap();
    h.runtime.cancel(parent).unwrap();
    h.runtime.cancel(child).unwrap();
    let _ = h.runtime.wait(parent).await;
    let _ = h.runtime.wait(child).await;
}
