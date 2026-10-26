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

#[tokio::test]
async fn shell_host_input_handoff_is_once_only_and_preserves_the_wire_prefix() {
    for wire in [Wire::ChatCompletions, Wire::Responses, Wire::Codex] {
        let directory = tempfile::tempdir().unwrap();
        let started = directory.path().join("started");
        let release = directory.path().join("release");
        let command = format!(
            "echo started > '{}'; while ! test -e '{}'; do sleep 0.01; done",
            started.display(),
            release.display()
        );
        let shell = Arc::new(tools::tools::Shell::new(Arc::new(
            sandbox::DirectSandbox::new_unchecked(),
        )));
        let gated = Arc::new(GatedRead {
            started: Notify::new(),
            release: Notify::new(),
            child_started: Notify::new(),
        });
        let mut harness = harness_with_wire(
            wire,
            vec![
                ScriptedResponse::tool_call(
                    "shell-start",
                    MODEL,
                    0,
                    "shell-start",
                    "shell",
                    [json!({"command":command}).to_string()],
                ),
                read_response(0),
                text_response("cache reuse after handoff"),
            ],
            1_000_000,
            gated.clone(),
            vec![shell.clone()],
            None,
            0.5,
            |_| {},
        );
        let (_storage, config) = attach_store(&mut harness);
        let run = harness
            .runtime
            .delegate_chat(
                "shell-handoff",
                Role::Worker,
                "initial".into(),
                chat_config("initial", true),
            )
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !started.exists() {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        harness
            .runtime
            .send_message_with_images(
                run,
                "unique shell handoff input".into(),
                vec![],
                Some("handoff".into()),
            )
            .unwrap();
        // Pause the second tool after the handoff request. Finish and observe
        // the real job before allowing Stop, which correctly refuses unobserved
        // jobs even if a model has emitted final text.
        tokio::time::timeout(std::time::Duration::from_secs(5), gated.started.notified())
            .await
            .unwrap();
        std::fs::write(&release, "").unwrap();
        let request = harness
            .mock
            .recorded_requests()
            .into_iter()
            .find(|r| r.path == wire.path() && r.body.to_string().contains("shell job: "))
            .unwrap()
            .body
            .to_string();
        let handle = request
            .split("shell job: ")
            .nth(1)
            .unwrap()
            .split(|c: char| !c.is_ascii_alphanumeric() && c != '-')
            .next()
            .unwrap();
        let ctx = tools::ToolExecutionContext {
            run_id: run.to_string(),
            ..Default::default()
        };
        shell
            .execute_with_context(&ctx, json!({"action":"poll", "job_id":handle}))
            .await
            .unwrap();
        gated.release.notify_one();
        let mut events = through_phase(&mut harness.receiver, run, AgentRunPhase::Waiting).await;
        let requests: Vec<_> = harness
            .mock
            .recorded_requests()
            .into_iter()
            .filter(|r| r.path == wire.path())
            .map(|r| r.body)
            .collect();
        assert_eq!(requests.len(), 3);
        for request in &requests[1..] {
            assert_eq!(
                request
                    .to_string()
                    .matches("unique shell handoff input")
                    .count(),
                1
            );
            assert!(request.to_string().contains("User input arrived"));
            assert!(request.to_string().contains("command was not restarted"));
        }
        assert_eq!(
            receipt(&harness, "handoff").status,
            storage::HostInputStatus::Delivered
        );
        let record = storage::Database::open(&config)
            .unwrap()
            .run_context(&run.to_string())
            .unwrap()
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Vec<String>>(&record.host_inputs_json).unwrap(),
            ["initial", "handoff"]
        );
        std::fs::write(release, "").unwrap();
        harness
            .runtime
            .stop(run, runtime::StopScope::SelfOnly)
            .unwrap();
        events.extend(through_phase(&mut harness.receiver, run, AgentRunPhase::Stopped).await);
        verify_trace(&harness, run, &events, 0);
    }
}
