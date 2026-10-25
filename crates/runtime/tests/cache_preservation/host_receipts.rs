//! Durable delivery boundaries never authorize replacing an already-sent prefix.
use super::*;

// A broken delivery boundary must fail rather than waiting forever for Error.
async fn through_phase(
    receiver: &mut EventReceiver,
    run: RunId,
    phase: AgentRunPhase,
) -> Vec<Event> {
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        super::through_phase(receiver, run, phase),
    )
    .await
    .expect("receipt scenario did not reach its expected phase")
}

fn attach_store(harness: &mut Harness) -> (storage::Storage, storage::StorageConfig) {
    let config = storage::StorageConfig {
        db_path: harness._directory.path().join("host-receipts.db"),
        ..Default::default()
    };
    let storage = storage::Storage::open(config.clone()).unwrap();
    harness.runtime = harness
        .runtime
        .clone()
        .with_run_store(runtime::RunStore::open(&config, storage.handle()).unwrap());
    (storage, config)
}

fn receipt(harness: &Harness, id: &str) -> storage::HostInputReceipt {
    harness
        .runtime
        .host_input_receipts(&[id.into()])
        .unwrap()
        .pop()
        .unwrap()
}

fn chat_config(id: &str, keep_alive: bool) -> RunConfig {
    RunConfig {
        initial_prompt_id: Some(id.into()),
        conversation: true,
        interactive: keep_alive,
        keep_alive,
        ..Default::default()
    }
}

#[tokio::test]
async fn done_resume_records_new_prompt_without_changing_the_wire_prefix() {
    for wire in [Wire::ChatCompletions, Wire::Responses, Wire::Codex] {
        let mut harness = harness_with_wire(
            wire,
            vec![
                text_response("first answer"),
                text_response("next answer"),
                text_response("cache reused"),
            ],
            1_000_000,
            Arc::new(BulkRead),
            vec![],
            None,
            0.5,
            |_| {},
        );
        let (_storage, config) = attach_store(&mut harness);
        let run = harness
            .runtime
            .delegate_chat(
                "receipts",
                Role::Worker,
                "initial".into(),
                chat_config("first", false),
            )
            .unwrap();
        let mut events = through_phase(&mut harness.receiver, run, AgentRunPhase::Done).await;
        assert_eq!(
            receipt(&harness, "first").status,
            storage::HostInputStatus::Delivered
        );
        harness
            .runtime
            .continue_goal(run, "new input only".into(), chat_config("second", false))
            .unwrap();
        // The prompt is delivered by the restore transaction before spawn.
        assert_eq!(
            receipt(&harness, "second").status,
            storage::HostInputStatus::Delivered
        );
        events.extend(through_phase(&mut harness.receiver, run, AgentRunPhase::Waiting).await);

        let record = storage::Database::open(&config)
            .unwrap()
            .run_context(&run.to_string())
            .unwrap()
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Vec<String>>(&record.host_inputs_json).unwrap(),
            ["first", "second"]
        );
        harness
            .runtime
            .send_message_with_images(
                run,
                "verify cache reuse".into(),
                vec![],
                Some("third".into()),
            )
            .unwrap();
        events.extend(through_phase(&mut harness.receiver, run, AgentRunPhase::Waiting).await);
        harness
            .runtime
            .stop(run, runtime::StopScope::SelfOnly)
            .unwrap();
        events.extend(through_phase(&mut harness.receiver, run, AgentRunPhase::Stopped).await);
        verify_trace(&harness, run, &events, 0);
    }
}

#[tokio::test]
async fn stopped_unread_input_is_dropped_then_only_the_resent_input_is_appended() {
    let gated = Arc::new(GatedRead {
        started: Notify::new(),
        release: Notify::new(),
        child_started: Notify::new(),
    });
    let mut harness = harness_with_read(
        vec![
            read_response(0),
            text_response("resent answer"),
            text_response("cache reused"),
        ],
        1_000_000,
        gated.clone(),
    );
    let (_storage, config) = attach_store(&mut harness);
    let run = harness
        .runtime
        .delegate_chat(
            "receipts",
            Role::Worker,
            "initial".into(),
            chat_config("first", false),
        )
        .unwrap();
    gated.started.notified().await;
    harness
        .runtime
        .send_message_with_images(
            run,
            "edit and resend".into(),
            vec![],
            Some("dropped".into()),
        )
        .unwrap();
    harness
        .runtime
        .stop(run, runtime::StopScope::SelfOnly)
        .unwrap();
    let mut events = through_phase(&mut harness.receiver, run, AgentRunPhase::Stopped).await;
    assert_eq!(
        receipt(&harness, "dropped").status,
        storage::HostInputStatus::Dropped
    );
    harness
        .runtime
        .continue_goal(run, "edit and resend".into(), chat_config("resent", false))
        .unwrap();
    events.extend(through_phase(&mut harness.receiver, run, AgentRunPhase::Waiting).await);

    let record = storage::Database::open(&config)
        .unwrap()
        .run_context(&run.to_string())
        .unwrap()
        .unwrap();
    let ids: Vec<String> = serde_json::from_str(&record.host_inputs_json).unwrap();
    assert_eq!(ids, ["first", "resent"]);
    assert_eq!(record.messages_json.matches("edit and resend").count(), 1);
    harness
        .runtime
        .send_message_with_images(
            run,
            "verify cache reuse".into(),
            vec![],
            Some("third".into()),
        )
        .unwrap();
    events.extend(through_phase(&mut harness.receiver, run, AgentRunPhase::Waiting).await);
    harness
        .runtime
        .stop(run, runtime::StopScope::SelfOnly)
        .unwrap();
    events.extend(through_phase(&mut harness.receiver, run, AgentRunPhase::Stopped).await);
    verify_trace(&harness, run, &events, 0);
}

#[tokio::test]
async fn failed_snapshot_unknown_then_resend_preserves_the_wire_prefix() {
    let mut harness = harness(
        vec![
            text_response("first answer"),
            text_response("resent answer"),
            text_response("cache reused"),
        ],
        1_000_000,
    );
    let (_storage, config) = attach_store(&mut harness);
    let run = harness
        .runtime
        .delegate_chat(
            "receipts",
            Role::Worker,
            "initial".into(),
            chat_config("first", true),
        )
        .unwrap();
    let mut events = through_phase(&mut harness.receiver, run, AgentRunPhase::Waiting).await;
    let conn = rusqlite::Connection::open(&config.db_path).unwrap();
    conn.execute_batch("CREATE TRIGGER fail_snapshot BEFORE UPDATE ON run_contexts BEGIN SELECT RAISE(FAIL, 'injected snapshot failure'); END;").unwrap();
    harness
        .runtime
        .send_message_with_images(
            run,
            "uncertain input".into(),
            vec![],
            Some("unknown".into()),
        )
        .unwrap();
    events.extend(through_phase(&mut harness.receiver, run, AgentRunPhase::Error).await);
    assert_eq!(
        receipt(&harness, "unknown").status,
        storage::HostInputStatus::Unknown
    );
    assert!(!events.iter().any(|event| matches!(&event.kind, EventKind::Lifecycle(LifecycleEvent::HostInputDelivered { prompt_id, .. }) if prompt_id == "unknown")));
    conn.execute_batch("DROP TRIGGER fail_snapshot;").unwrap();
    harness
        .runtime
        .continue_goal(run, "uncertain input".into(), chat_config("resent", false))
        .unwrap();
    events.extend(through_phase(&mut harness.receiver, run, AgentRunPhase::Waiting).await);

    let record = storage::Database::open(&config)
        .unwrap()
        .run_context(&run.to_string())
        .unwrap()
        .unwrap();
    assert_eq!(record.messages_json.matches("uncertain input").count(), 1);
    assert_eq!(
        serde_json::from_str::<Vec<String>>(&record.host_inputs_json).unwrap(),
        ["first", "resent"]
    );
    harness
        .runtime
        .send_message_with_images(
            run,
            "verify cache reuse".into(),
            vec![],
            Some("third".into()),
        )
        .unwrap();
    events.extend(through_phase(&mut harness.receiver, run, AgentRunPhase::Waiting).await);
    harness
        .runtime
        .stop(run, runtime::StopScope::SelfOnly)
        .unwrap();
    events.extend(through_phase(&mut harness.receiver, run, AgentRunPhase::Stopped).await);
    verify_trace(&harness, run, &events, 0);
}
