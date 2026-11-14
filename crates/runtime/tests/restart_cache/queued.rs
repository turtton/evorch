use super::*;

#[tokio::test]
async fn queued_launch_runs_once_then_preserves_the_wire_prefix() {
    for wire in [Wire::ChatCompletions, Wire::Responses, Wire::Codex] {
        let directory = tempfile::tempdir().unwrap();
        let config = storage::StorageConfig {
            db_path: directory.path().join("queued.db"),
            ..Default::default()
        };
        let storage = storage::Storage::open(config.clone()).unwrap();
        let mut h = harness_with_wire(
            wire,
            vec![
                read_response(0),
                text_response("first complete"),
                read_response(1),
                text_response("second complete"),
            ],
            1_000_000,
            Arc::new(BulkRead),
            Vec::new(),
            None,
            0.9,
            |_| {},
        );
        h.runtime = h
            .runtime
            .with_run_store(runtime::RunStore::open(&config, storage.handle()).unwrap());
        let attempt = h.runtime.begin_restart_quiesce().unwrap();
        let run = h.runtime.delegate_background(
            Role::Worker,
            "queued prompt exactly once".into(),
            RunConfig {
                interactive: true,
                keep_alive: true,
                ..Default::default()
            },
        );
        assert!(h.runtime.list_agents().is_empty());
        assert!(h.mock.recorded_requests().is_empty());
        h.runtime.cancel_restart_quiesce(&attempt).unwrap();
        let mut events = through_phase(&mut h.receiver, run, AgentRunPhase::Waiting).await;
        assert!(h.runtime.cancel_restart_quiesce(&attempt).is_err());
        h.runtime
            .send_message(run, "continue normally".into())
            .unwrap();
        events.extend(through_phase(&mut h.receiver, run, AgentRunPhase::Waiting).await);
        verify_trace(&h, run, &events, 0);
        let requests = h.mock.recorded_requests();
        let last = requests
            .iter()
            .rev()
            .find(|r| r.path == h.wire.path())
            .unwrap();
        assert_eq!(
            last.body
                .to_string()
                .matches("queued prompt exactly once")
                .count(),
            1
        );
        h.runtime.stop(run, runtime::StopScope::SelfOnly).unwrap();
        h.runtime.wait(run).await.unwrap();
    }
}
