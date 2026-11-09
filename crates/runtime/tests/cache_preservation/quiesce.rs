//! Reconnection is not a prompt replacement boundary.
use super::*;

#[tokio::test]
async fn safe_point_checkpoint_reuses_exact_prefix_after_reconnection() {
    for wire in [Wire::ChatCompletions, Wire::Responses, Wire::Codex] {
        for compact in [false, true] {
            eprintln!(
                "safe-point cache fixture: {}, compact={compact}",
                wire.provider_type()
            );
            let mut script = if compact {
                vec![
                    text_response(&"old analysis ".repeat(100)),
                    text_response(&"continued analysis ".repeat(100)),
                    text_response("new prefix"),
                    text_response("second"),
                    text_response("third"),
                ]
            } else {
                vec![
                    text_response("first"),
                    text_response("second"),
                    text_response("third"),
                ]
            };
            if compact && matches!(wire, Wire::Codex) {
                // Codex performs official compaction on the Responses endpoint;
                // this is a real extra request, not an ordinary model turn.
                script.insert(
                    2,
                    ScriptedResponse::compaction(
                        "safe-point-compaction",
                        MODEL,
                        "safe-point-opaque-prefix",
                    ),
                );
            }
            let mut harness = harness_with_wire(
                wire,
                script,
                1_000_000,
                Arc::new(BulkRead),
                vec![],
                None,
                0.5,
                |_| {},
            );
            let config = storage::StorageConfig {
                db_path: harness._directory.path().join("safe-point.db"),
                ..Default::default()
            };
            let storage = storage::Storage::open(config.clone()).unwrap();
            harness.runtime = harness
                .runtime
                .clone()
                .with_run_store(runtime::RunStore::open(&config, storage.handle()).unwrap());
            let run_config = RunConfig {
                conversation: true,
                interactive: true,
                keep_alive: true,
                ..Default::default()
            };
            let run = harness
                .runtime
                .delegate_chat(
                    "safe-point",
                    Role::Worker,
                    "start".into(),
                    run_config.clone(),
                )
                .unwrap();
            let mut events =
                through_phase(&mut harness.receiver, run, AgentRunPhase::Waiting).await;
            if compact {
                // Send the long response once before replacing its prefix. Without
                // this turn no provider ever saw the text the compaction removes.
                harness
                    .runtime
                    .send_message(run, "expand original prefix".into())
                    .unwrap();
                events.extend(
                    through_phase(&mut harness.receiver, run, AgentRunPhase::Waiting).await,
                );
                harness.runtime.compact(run).unwrap();
                harness
                    .runtime
                    .send_message(run, "warm the compacted prefix".into())
                    .unwrap();
                events.extend(
                    through_phase(&mut harness.receiver, run, AgentRunPhase::Waiting).await,
                );
            }
            harness.runtime.quiesce().await.unwrap();
            harness
                .runtime
                .send_message_with_images(
                    run,
                    "pending across restart".into(),
                    vec![],
                    Some("pending-safe-point".into()),
                )
                .unwrap();
            let intent = harness
                .runtime
                .resume_intents()
                .unwrap()
                .into_iter()
                .find(|intent| intent.run_id == run)
                .unwrap();
            assert_eq!(intent.compactions.len(), usize::from(compact));
            let checkpoint = intent.checkpoint.as_ref().unwrap();
            let record = storage::Database::open(&config)
                .unwrap()
                .run_context(&run.to_string())
                .unwrap()
                .unwrap();
            assert!(checkpoint.snapshot.available(Some(&record)));
            let saved = intent.provider_input.as_ref().unwrap();
            assert!(!saved.request.messages.is_empty());
            assert!(!saved.request.tools.is_empty());
            // Independent wire identity: the saved key is exactly the one sent,
            // including Codex's deterministic UUID, not a newly minted incarnation.
            let requests = harness.mock.recorded_requests();
            let first = requests
                .iter()
                .find(|request| request.path == wire.path())
                .unwrap();
            match wire {
                Wire::ChatCompletions => assert_eq!(saved.affinity_key, run.to_string()),
                Wire::Responses | Wire::Codex => {
                    assert_eq!(first.body["prompt_cache_key"], saved.affinity_key)
                }
            }
            let round_trip: runtime::quiesce::ResumeIntent =
                serde_json::from_str(&serde_json::to_string(&intent).unwrap()).unwrap();
            assert_eq!(round_trip, intent);
            // The old task stays parked (like a process that has stopped executing).
            // A fresh runtime has no live handles or admission of the old incarnation.
            let old = harness.runtime.clone();
            restart_runtime(&mut harness, &config, &storage);
            harness.runtime.resume_chat(run, run_config).unwrap();
            events.extend(through_phase(&mut harness.receiver, run, AgentRunPhase::Waiting).await);
            harness
                .runtime
                .send_message(run, "next input".into())
                .unwrap();
            events.extend(through_phase(&mut harness.receiver, run, AgentRunPhase::Waiting).await);
            harness
                .runtime
                .send_message(run, "another input".into())
                .unwrap();
            events.extend(through_phase(&mut harness.receiver, run, AgentRunPhase::Waiting).await);
            // Reconnect is not permission for a ModelSwitch re-summary.
            assert!(!events.iter().any(|event| matches!(
                &event.kind,
                EventKind::Compaction(CompactionEvent::Compacted {
                    reason: CompactionReason::ModelSwitch,
                    ..
                })
            )));
            // Both verifiers retain input-derived cache accounting and independent
            // HTTP prefix checks; the official call also reports compaction usage.
            if compact && matches!(wire, Wire::Codex) {
                verify_official_compaction_trace(&harness, run, &events);
            } else {
                verify_trace(&harness, run, &events, usize::from(compact));
            }
            let resumed = harness
                .runtime
                .resume_intents()
                .unwrap()
                .into_iter()
                .find(|intent| intent.run_id == run)
                .unwrap();
            assert!(
                resumed
                    .inputs
                    .iter()
                    .any(|input| input.id == "pending-safe-point"
                        && input.text == "pending across restart")
            );
            harness.runtime.cancel(run).unwrap();
            harness.runtime.wait(run).await.unwrap();
            old.cancel(run).unwrap();
            old.wait(run).await.unwrap();
        }
    }
}
