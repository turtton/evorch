//! Prepare/cancel is not a provider history replacement boundary.
use super::*;
mod queued;

#[tokio::test]
async fn two_restart_cancellations_keep_wire_prefix_and_parked_input_once() {
    for wire in [Wire::ChatCompletions, Wire::Responses, Wire::Codex] {
        let directory = tempfile::tempdir().unwrap();
        let config = storage::StorageConfig {
            db_path: directory.path().join("restart.db"),
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
                text_response("third complete"),
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
        let run = h.runtime.delegate_background(
            Role::Worker,
            "Preserve the already-sent prefix".into(),
            RunConfig {
                interactive: true,
                keep_alive: true,
                ..Default::default()
            },
        );
        let mut events = through_phase(&mut h.receiver, run, AgentRunPhase::Waiting).await;
        for index in 0..2 {
            let attempt = h.runtime.begin_restart_quiesce().unwrap();
            h.runtime.wait_quiesced().await.unwrap();
            if index == 0 {
                h.runtime
                    .send_message_with_images(
                        run,
                        "parked human suffix".into(),
                        Vec::new(),
                        Some("parked-once".into()),
                    )
                    .unwrap();
            }
            let fence = h.runtime.fence_dispatch_restart().await.unwrap();
            h.runtime.drain_dispatch_tails(&fence).await.unwrap();
            h.runtime.cancel_restart_quiesce(&attempt).unwrap();
            assert!(
                h.runtime
                    .resume_intents()
                    .unwrap()
                    .iter()
                    .all(|i| i.checkpoint.is_none())
            );
            if index == 1 {
                h.runtime
                    .send_message(run, "after the second cancellation".into())
                    .unwrap();
            }
            events.extend(through_phase(&mut h.receiver, run, AgentRunPhase::Waiting).await);
        }
        verify_trace(&h, run, &events, 0);
        let requests = h.mock.recorded_requests();
        let last = requests
            .iter()
            .rev()
            .find(|request| request.path == h.wire.path())
            .unwrap();
        assert_eq!(
            last.body.to_string().matches("parked human suffix").count(),
            1,
            "parked input must be consumed once, not replayed by cancellation"
        );
        h.runtime.stop(run, runtime::StopScope::SelfOnly).unwrap();
        h.runtime.wait(run).await.unwrap();
    }
}
