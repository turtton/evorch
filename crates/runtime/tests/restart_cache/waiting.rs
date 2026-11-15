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
        h.runtime.drain_dispatch_tails(&fence).await.unwrap();
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
        let mut session = h
            .runtime
            .begin_restart_capture_session(fence, &tx, recipes)
            .unwrap();
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
