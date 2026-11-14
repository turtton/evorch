use super::*;

#[tokio::test]
async fn queued_child_and_its_parent_notice_preserve_the_parent_wire_prefix() {
    for wire in [Wire::ChatCompletions, Wire::Responses, Wire::Codex] {
        let read = Arc::new(GatedRead {
            started: Notify::new(),
            release: Notify::new(),
            child_started: Notify::new(),
        });
        let mut h = harness_with_wire(
            wire,
            vec![read_response(0)],
            1_000_000,
            read.clone(),
            Vec::new(),
            None,
            0.9,
            |_| {},
        );
        let config = storage::StorageConfig {
            db_path: h._directory.path().join("queued-child.db"),
            ..Default::default()
        };
        let storage = storage::Storage::open(config.clone()).unwrap();
        h.runtime = h
            .runtime
            .with_run_store(runtime::RunStore::open(&config, storage.handle()).unwrap());
        let parent = h.runtime.delegate_background(
            Role::Worker,
            "coordinate".into(),
            RunConfig {
                interactive: true,
                keep_alive: true,
                ..Default::default()
            },
        );
        read.started.notified().await;
        let mut events = Vec::new();
        loop {
            let event = h.receiver.recv().await.unwrap();
            let started = matches!(&event.kind, EventKind::Tool(event_bus::ToolEvent::ToolStarted { run_id: Some(id), .. }) if id == &parent.to_string());
            events.push(event);
            if started {
                break;
            }
        }
        let attempt = h.runtime.begin_restart_quiesce().unwrap();
        let child = h
            .runtime
            .delegate_background_as_child(
                parent,
                Role::Worker,
                "queued child",
                RunConfig::default(),
            )
            .unwrap();
        assert_eq!(
            h.mock
                .recorded_requests()
                .iter()
                .filter(|request| request.path == wire.path())
                .count(),
            1,
            "queued child must not call its provider"
        );
        h.mock.enqueue(vec![
            text_response("queued child complete"),
            text_response("parent ready"),
            text_response("parent complete"),
            // A completion received in the tool batch is observed after its
            // ordinary response, before the later human input.
            text_response("after the child notice"),
        ]);
        h.runtime.cancel_restart_quiesce(&attempt).unwrap();
        // Only the child can reach the provider while the parent's read is held.
        h.runtime.wait(child).await.unwrap();
        // Usage has no run identity. This interval contains only the child's
        // provider call; the parent's read has not yet been released.
        events.extend(
            through_phase(&mut h.receiver, child, AgentRunPhase::Done)
                .await
                .into_iter()
                .filter(|event| !matches!(event.kind, EventKind::Usage(_))),
        );
        read.release.notify_one();
        events.extend(through_phase(&mut h.receiver, parent, AgentRunPhase::Waiting).await);
        h.runtime
            .send_message(parent, "continue after child".into())
            .unwrap();
        events.extend(through_phase(&mut h.receiver, parent, AgentRunPhase::Waiting).await);
        let mut requests = h
            .mock
            .recorded_requests()
            .into_iter()
            .filter(|request| request.path == wire.path())
            .map(|request| request.body)
            .collect::<Vec<_>>();
        assert_eq!(requests.len(), 5, "one child request, four parent requests");
        requests.remove(1);
        assert_eq!(
            requests[2]
                .to_string()
                .matches("queued child complete")
                .count(),
            1,
            "the child completion must be appended exactly once"
        );
        assert_eq!(h.mock.remaining_scripts(), 0);
        verify_trace_of(&h, parent, &events, 0, &requests);
        h.runtime
            .stop(parent, runtime::StopScope::SelfOnly)
            .unwrap();
        h.runtime.wait(parent).await.unwrap();
    }
}
