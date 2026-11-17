use super::*;

#[tokio::test]
async fn lane_roots_only_change_their_own_conversation() {
    let f = Fixture::new();
    let lead = f
        .start_role("lead", Role::Orchestrator, RunConfig::default())
        .await;
    f.add(lead);
    let brief = serde_json::from_value(json!({
        "objective":"independent work","inputs":[],"deliverables":["patch"],
        "acceptance_criteria":["review"],"constraints":[],"workspace":{},
        "report_policy":"attached"
    }))
    .unwrap();
    let lane = f.runtime.open_lane(lead, "create-lane", brief).unwrap();
    f.model.entered.notified().await;
    let lane_root = lane.root_run_id;
    assert_eq!(read(&f, lane_root)["revision"], 0);
    let initial = f.add(lane_root);
    let task = initial["created"][0][1].as_str().unwrap();
    let lead_before = read(&f, lead);
    let mailbox = f.mailbox(lane_root);
    let caller = f.caller(lane_root, &mailbox);
    let lead_task = lead_before["nodes"][0]["task_id"].as_str().unwrap();
    assert!(
        tools::read(
            &f.runtime,
            &caller,
            json!({"view":"task","task_id":lead_task})
        )
        .is_err()
    );
    tools::update(&f.runtime, &caller, json!({"expected_revision":1,"operations":[{"op":"update","task":{"id":task},"status":"completed"}]})).unwrap();
    assert_eq!(
        read(&f, lane_root)["progress"]["leaf_counts"]["completed"],
        1
    );
    assert_eq!(read(&f, lead), lead_before);
    f.stop(lane_root).await;
    assert_eq!(read(&f, lead), lead_before);
    f.stop(lead).await;
}

#[tokio::test]
async fn a_durable_thread_tombstone_refuses_live_root_reads_and_updates() {
    let f = Fixture::new();
    let run = f.start("thread").await;
    f.add(run);
    let conn = rusqlite::Connection::open(&f.config.db_path).unwrap();
    conn.execute("INSERT INTO thread_tombstones(thread_id, family_key, deleted_at_ns) VALUES ('thread','thread',1)", []).unwrap();
    let mailbox = f.mailbox(run);
    let caller = f.caller(run, &mailbox);
    assert!(
        tools::read(&f.runtime, &caller, json!({"view":"tree"}))
            .unwrap_err()
            .starts_with("thread_deleted:")
    );
    assert!(
        tools::update(&f.runtime, &caller, addition(1, "pending"))
            .unwrap_err()
            .starts_with("thread_deleted:")
    );
    assert!(!f.runtime.reconcile_plan_or_report(&caller));
    f.stop(run).await;
}

#[tokio::test]
async fn tool_results_fit_the_final_serialized_envelope_and_oversize_is_atomic() {
    let f = Fixture::new();
    let run = f.start("thread").await;
    let task = f.add(run)["created"][0][1].as_str().unwrap().to_owned();
    let mailbox = f.mailbox(run);
    let caller = f.caller(run, &mailbox);
    let input = |description: String| json!({"expected_revision":1,"operations":[{"op":"update","task":{"id":task},"description":description,"result":"r".repeat(4096),"blocker":"b".repeat(2048)}]});
    let before = read(&f, run);
    assert!(tools::update(&f.runtime, &caller, input("\u{0001}".repeat(4096))).is_err());
    assert_eq!(read(&f, run), before);
    tools::update(&f.runtime, &caller, input("d".repeat(4096))).unwrap();
    let detail = tools::read(&f.runtime, &caller, json!({"view":"task","task_id":task})).unwrap();
    assert_eq!(detail["node"]["description"], "d".repeat(4096));
    assert_eq!(detail["node"]["result"], "r".repeat(4096));
    assert!(
        serde_json::to_vec(&detail).unwrap().len()
            <= storage::conversation_plan::PLAN_TOOL_RESULT_MAX_BYTES
    );
    // An update acknowledgement also fits with all 32 maximum-length refs,
    // including JSON's six-byte control-character escapes. No refs are lost.
    let refs: Vec<_> = (0u8..32)
        .map(|n| format!("{}{}", "\u{0001}".repeat(63), char::from(n)))
        .collect();
    let operations: Vec<_> = refs
        .iter()
        .map(|client_ref| {
            json!({"op":"add","client_ref":client_ref,"title":"task","weight":1,"status":"pending"})
        })
        .collect();
    let acknowledgement = tools::update(
        &f.runtime,
        &caller,
        json!({"expected_revision":2,"operations":operations}),
    )
    .unwrap();
    assert_eq!(
        acknowledgement["created"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry[0].as_str().unwrap().to_owned())
            .collect::<Vec<_>>(),
        refs
    );
    assert!(
        serde_json::to_vec(&acknowledgement).unwrap().len()
            <= storage::conversation_plan::PLAN_TOOL_RESULT_MAX_BYTES
    );
    f.stop(run).await;
}

#[tokio::test]
async fn durable_handoff_transfers_plan_without_interrupt_or_snapshot_event() {
    let f = Fixture::new();
    let source = f.start("source").await;
    f.add(source);
    let publication = f.runtime.execution_snapshot(source).unwrap().publication();
    let config = RunConfig {
        conversation: true,
        ..Default::default()
    };
    let memo = crate::EscalationMemo {
        source_run_id: source,
        original_request: "original request".into(),
        escalation_reason: "coordinate".into(),
        findings: vec![],
        files_touched: vec![],
        blockers: vec![],
        workspace_state: String::new(),
        suggested_next: String::new(),
    };
    let target = f
        .runtime
        .prepare_handoff_to_lane(&memo, &config, Default::default(), &publication)
        .unwrap();
    let thread = event_bus::escalation_thread_id(&target.to_string());
    assert_eq!(
        f.runtime
            .conversation_plan_summary("source")
            .unwrap()
            .revision,
        0
    );
    f.stop(source).await;
    assert_eq!(
        f.runtime
            .start_committed_handoff(source, &publication.incarnation(), &config)
            .unwrap(),
        target
    );
    f.model.entered.notified().await;
    let plan = read(&f, target);
    assert_eq!(plan["revision"], 1);
    assert_eq!(plan["progress"]["leaf_counts"]["in_progress"], 1);
    assert_eq!(
        f.runtime
            .conversation_plan_history(&thread, None, None)
            .unwrap()
            .entries
            .len(),
        1
    );
    f.stop(target).await;
}
