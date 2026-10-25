//! Reads that gather a thread's runs, requests and events from the user database.

use event_bus::{Event, LifecycleEvent, MessageEvent, ToolEvent};
use storage::usage::{UsageRequestRecord, UsageStatus};
use storage::{Database, RunContextRecord, Storage, StorageConfig};

fn fixture() -> (tempfile::TempDir, Storage, Database) {
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("thread-runs.sqlite3"),
        ..Default::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let database = Database::open(&config).unwrap();
    (dir, storage, database)
}

fn context(run_id: &str, parent_run_id: Option<&str>) -> RunContextRecord {
    RunContextRecord {
        run_id: run_id.into(),
        role: "worker".into(),
        name: run_id.into(),
        parent_run_id: parent_run_id.map(str::to_owned),
        config_json: "{}".into(),
        messages_json: "[]".into(),
        checkpoints_json: "[]".into(),
        terminal_phase: "Done".into(),
        restorable: false,
        updated_at_ns: 1,
        host_inputs_json: "[]".into(),
    }
}

fn request(id: &str, run_id: &str, parent_run_id: Option<&str>) -> UsageRequestRecord {
    UsageRequestRecord {
        request_id: id.into(),
        at_ns: 1,
        provider: "openai".into(),
        profile: Some("main".into()),
        model: "gpt-test".into(),
        run_id: Some(run_id.into()),
        parent_run_id: parent_run_id.map(str::to_owned),
        role: Some("worker".into()),
        purpose: Some("agent".into()),
        status: UsageStatus::Ok,
        failure: None,
        finish_reason: Some("stop".into()),
        input_tokens: 10,
        output_tokens: 1,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        reasoning_tokens: None,
        ttft_ms: None,
        duration_ms: 1,
        cost_usd: None,
        service_tier: None,
    }
}

#[test]
fn descendants_follow_context_and_ledger_parent_links() {
    // Given: a root whose child has a saved context and whose grandchild only
    // appears in the usage ledger, beside an unrelated run.
    let (_dir, storage, database) = fixture();
    let handle = storage.handle();
    handle.upsert_run_context(&context("root", None)).unwrap();
    handle
        .upsert_run_context(&context("child", Some("root")))
        .unwrap();
    handle.upsert_run_context(&context("other", None)).unwrap();
    handle
        .record_usage_requests(vec![
            request("r1", "grandchild", Some("child")),
            request("r2", "other", None),
        ])
        .unwrap();

    // When: the root's runs are expanded.
    let runs = database.runs_with_descendants(&["root".into()]).unwrap();

    // Then: the whole tree is returned and nothing else.
    assert_eq!(runs, ["child", "grandchild", "root"]);
    let requests = database.usage_requests_for_runs(&runs).unwrap();
    assert_eq!(
        requests
            .iter()
            .map(|row| row.record.request_id.as_str())
            .collect::<Vec<_>>(),
        ["r1"]
    );
}

#[test]
fn events_for_runs_match_run_fields_and_approval_call_ids_only() {
    // Given: events of the run, an approval of it, an event of another run that
    // mentions the run only in its text, and an event of a run with a longer id.
    let (_dir, storage, database) = fixture();
    let handle = storage.handle();
    let events = [
        Event::new(LifecycleEvent::AgentRunStarted {
            run_id: "run-1".into(),
            parent_run_id: None,
            agent_name: "worker".into(),
            role: "worker".into(),
        }),
        Event::new(ToolEvent::ApprovalRequested {
            tool_name: "shell".into(),
            call_id: "run-1:call-1".into(),
            input: None,
        }),
        Event::new(MessageEvent::MessageCompleted {
            run_id: "run-2".into(),
            text: "\"run_id\":\"run-1\"".into(),
        }),
        Event::new(MessageEvent::MessageCompleted {
            run_id: "run-10".into(),
            text: "later".into(),
        }),
    ];
    for event in &events {
        handle.append_event(Some("gui"), event).unwrap();
    }

    // When: the run's events are read, without and with a limit.
    let (all, truncated) = database.events_for_runs(&["run-1".into()], 0, 10).unwrap();
    let (first, more) = database.events_for_runs(&["run-1".into()], 0, 1).unwrap();

    // Then: only the run's own and its approval's events match, oldest first.
    assert_eq!(
        all.iter()
            .map(|stored| &stored.event.kind)
            .collect::<Vec<_>>(),
        [&events[0].kind, &events[1].kind]
    );
    assert!(!truncated);
    assert_eq!(first.len(), 1);
    assert!(more);
}
