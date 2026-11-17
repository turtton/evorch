//! Plans live in the projection, not in a rewritten prompt snapshot. After a
//! context boundary the model reads the current plan through the public tool.
use super::*;

pub(super) const NOTICE: &str = "[runtime conversation plan state]";

pub(super) fn store(h: &mut Harness) -> (storage::StorageConfig, storage::Storage) {
    let config = storage::StorageConfig {
        db_path: h._directory.path().join("plan-cache.db"),
        ..Default::default()
    };
    let db = storage::Storage::open(config.clone()).unwrap();
    h.runtime = h
        .runtime
        .clone()
        .with_run_store(runtime::RunStore::open(&config, db.handle()).unwrap());
    (config, db)
}

pub(super) fn update(id: &str, revision: u64, operations: Value) -> ScriptedResponse {
    call(
        id,
        "plan_update",
        json!({"expected_revision":revision,"operations":operations}),
    )
}

pub(super) fn add(title: &str, status: &str) -> Value {
    json!({"op":"add","client_ref":title,"title":title,"status":status})
}

pub(super) fn read(id: &str) -> ScriptedResponse {
    call(id, "plan_read", json!({"view":"tree"}))
}

pub(super) fn result(request: &Value, id: &str) -> Value {
    let encoded: Value = serde_json::from_str(&tool_result_text(request, id)).unwrap();
    serde_json::from_str(encoded.as_str().expect("text ToolResult")).unwrap()
}

pub(super) fn age_record(config: &storage::StorageConfig, run: RunId) {
    let connection = rusqlite::Connection::open(&config.db_path).unwrap();
    assert_eq!(connection.execute(
        "UPDATE run_contexts SET config_json = json_remove(config_json, '$.plan_notice'), config_len = length(CAST(json_remove(config_json, '$.plan_notice') AS BLOB)) WHERE run_id = ?1",
        [run.to_string()],
    ).unwrap(), 1);
}

pub(super) fn notices(request: &Value) -> Vec<String> {
    user_contents(request)
        .into_iter()
        .filter(|text| text.starts_with(NOTICE))
        .collect()
}

/// A fork may reuse the source's common prefix even on its first request.
/// Check each root's actual usage and wire, without assuming that first hit is cold.
pub(super) fn verify_root_prefix(h: &Harness, run: RunId, events: &[Event], requests: &[Value]) {
    let usage = completed_requests(events, run);
    assert_eq!(usage.len(), requests.len());
    assert!(
        usage
            .iter()
            .all(|(input, cached)| *input > 0 && cached <= input)
    );
    for (index, pair) in requests.windows(2).enumerate() {
        assert_append_only(h.wire.protocol(), &pair[0], &pair[1]).unwrap();
        assert!(usage[index + 1].1 >= usage[index].0);
    }
    assert!(events.iter().all(|e| !matches!(&e.kind,
        EventKind::Diagnostic(d) if d.code=="CacheRegression")));
}

#[tokio::test]
async fn plan_replacement_parallel_tasks_and_clear_preserve_the_wire_prefix() {
    let mut h = harness(
        vec![
            update(
                "parallel",
                0,
                json!([
                    add("Implementation", "in_progress"),
                    add("Interface", "in_progress")
                ]),
            ),
            read("parallel-read"),
            update("replace-clear", 1, json!([{"op":"clear"}])),
            update("replacement", 2, json!([add("Verify changes", "pending")])),
            read_response(2),
            update("clear", 3, json!([{"op":"clear"}])),
            read("cleared-read"),
            text_response("ready"),
        ],
        1_000_000,
    );
    let (_config, _db) = store(&mut h);
    let run = h
        .runtime
        .delegate_chat(
            "plan-cache",
            Role::Worker,
            "Inspect and verify changes".into(),
            RunConfig {
                conversation: true,
                ..Default::default()
            },
        )
        .unwrap();
    let events = through_phase(&mut h.receiver, run, AgentRunPhase::Done).await;
    let current = h.runtime.conversation_plan_summary("plan-cache").unwrap();
    assert_eq!(current.revision, 4);
    assert_eq!(current.progress.node_count, 0);
    assert!(h.runtime.thread_goal("plan-cache").is_none());
    let requests = chat_requests(&h);
    assert_eq!(
        user_contents(&requests[0]),
        vec!["Inspect and verify changes"]
    );
    let parallel = result(&requests[2], "parallel-read");
    assert_eq!(parallel["revision"], 1);
    assert_eq!(parallel["nodes"].as_array().unwrap().len(), 2);
    assert!(
        parallel["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .all(|n| n["status"] == "in_progress")
    );
    let cleared = result(requests.last().unwrap(), "cleared-read");
    assert_eq!(cleared["revision"], 4);
    assert_eq!(cleared["nodes"], json!([]));
    assert_eq!(parallel["plan_id"], cleared["plan_id"]);
    assert!(
        requests.iter().all(|r| notices(r).is_empty()),
        "new conversations need no switch notice"
    );
    verify_trace(&h, run, &events, 0);
}

#[tokio::test]
async fn plan_rejections_paging_and_storage_retry_preserve_the_wire_prefix() {
    let mut h = harness(
        vec![
            update(
                "initial",
                0,
                json!([add("First", "pending"), add("Second", "pending")]),
            ),
            call("page-one", "plan_read", json!({"view":"tree","limit":1})),
            text_response("first page read"),
        ],
        1_000_000,
    );
    let (config, _db) = store(&mut h);
    let run = h
        .runtime
        .delegate_chat(
            "paged-plan",
            Role::Worker,
            "Inspect the paged plan".into(),
            RunConfig {
                conversation: true,
                interactive: true,
                keep_alive: true,
                ..Default::default()
            },
        )
        .unwrap();
    let mut events = through_phase(&mut h.receiver, run, AgentRunPhase::Waiting).await;
    let page = result(chat_requests(&h).last().unwrap(), "page-one");
    let cursor = page["next_cursor"]
        .as_str()
        .expect("second page")
        .to_owned();
    let task = page["nodes"][0]["task_id"].as_str().unwrap().to_owned();
    h.mock.enqueue(vec![
        update("stale",0,json!([{"op":"clear"}])),
        update("oversize",1,json!([{"op":"update","task":{"id":task},"description":"x".repeat(storage::conversation_plan::PLAN_NODE_DETAIL_MAX_BYTES+1)}])),
        call("page-two","plan_read",json!({"view":"tree","limit":1,"cursor":cursor})),
        update("advance",1,json!([{"op":"update","task":{"id":task},"status":"completed"}])),
        call("stale-page","plan_read",json!({"view":"tree","limit":1,"cursor":cursor})),
        text_response("rejections observed"),
    ]);
    h.runtime
        .send_message(run, "Read next page and advance".into())
        .unwrap();
    events.extend(through_phase(&mut h.receiver, run, AgentRunPhase::Waiting).await);
    let request = chat_requests(&h).last().unwrap().clone();
    assert!(tool_result_text(&request, "stale").contains("stale_revision"));
    assert!(tool_result_text(&request, "oversize").contains("plan_rejected"));
    assert!(tool_result_text(&request, "stale-page").contains("stale_cursor"));
    let second = result(&request, "page-two");
    assert_eq!(second["revision"], 1);
    assert_ne!(second["nodes"][0]["task_id"], task);
    assert!(second["next_cursor"].is_null());
    assert_eq!(
        h.runtime
            .conversation_plan_summary("paged-plan")
            .unwrap()
            .revision,
        2
    );

    // Inject a transaction failure without killing the writer or racing a
    // running tool. The next human turn occurs only after the trigger exists.
    let conn = rusqlite::Connection::open(&config.db_path).unwrap();
    conn.execute_batch("CREATE TRIGGER fail_plan_cache_update BEFORE UPDATE ON conversation_plans BEGIN SELECT RAISE(ABORT, 'injected plan write failure'); END;").unwrap();
    h.mock.enqueue(vec![
        update("storage-error", 2, json!([{"op":"clear"}])),
        text_response("write refused"),
    ]);
    h.runtime.send_message(run, "Attempt clear".into()).unwrap();
    events.extend(through_phase(&mut h.receiver, run, AgentRunPhase::Waiting).await);
    assert!(
        tool_result_text(chat_requests(&h).last().unwrap(), "storage-error")
            .contains("storage_unavailable")
    );
    assert_eq!(
        h.runtime
            .conversation_plan_summary("paged-plan")
            .unwrap()
            .revision,
        2
    );
    conn.execute_batch("DROP TRIGGER fail_plan_cache_update")
        .unwrap();
    h.mock.enqueue(vec![
        read("before-retry"),
        update("retry", 2, json!([{"op":"clear"}])),
        read("after-retry"),
        text_response("done"),
    ]);
    h.runtime
        .send_message(run, "Re-read and retry".into())
        .unwrap();
    events.extend(through_phase(&mut h.receiver, run, AgentRunPhase::Waiting).await);
    let final_request = chat_requests(&h).last().unwrap().clone();
    assert_eq!(result(&final_request, "before-retry")["revision"], 2);
    assert_eq!(result(&final_request, "after-retry")["revision"], 3);
    assert_eq!(result(&final_request, "after-retry")["nodes"], json!([]));
    h.runtime.cancel(run).unwrap();
    h.runtime.wait(run).await.unwrap();
    verify_trace(&h, run, &events, 0);
}

async fn plan_compaction_reuses_new_prefix(reason: CompactionReason, cleared: bool) {
    let review = support::ReviewStorage::new();
    let automatic = reason == CompactionReason::Automatic;
    let mut script = vec![
        update("obsolete", 0, json!([add("Superseded task", "pending")])),
        update("replace", 1, json!([{"op":"clear"}])),
    ];
    if !cleared {
        script.push(update(
            "latest",
            2,
            json!([add("Latest authoritative task", "pending")]),
        ));
    }
    script.push(text_response(
        &"old analysis that can be summarized ".repeat(if automatic { 6000 } else { 100 }),
    ));
    if reason == CompactionReason::Agent {
        script.push(call("compact-plan", "compact", json!({})));
    }
    script.extend([
        read("current-plan"),
        read_response(2),
        text_response("done"),
    ]);
    let mut h = harness(script, if automatic { 64_000 } else { 1_000_000 });
    let (_config, _db) = store(&mut h);
    let run = h
        .runtime
        .delegate_chat(
            "compacted-plan",
            if reason == CompactionReason::Agent {
                Role::Orchestrator
            } else {
                Role::Worker
            },
            "Inspect changes".into(),
            RunConfig {
                conversation: true,
                interactive: true,
                ..Default::default()
            },
        )
        .unwrap();
    let mut events = through_phase(&mut h.receiver, run, AgentRunPhase::Waiting).await;
    let before = h
        .runtime
        .conversation_plan_tree("compacted-plan", None, None)
        .unwrap();
    if reason == CompactionReason::Manual {
        h.runtime.compact(run).unwrap();
    }
    review.attach(&h.runtime);
    h.runtime
        .send_message(run, "Continue after the context boundary".into())
        .unwrap();
    events.extend(through_phase(&mut h.receiver, run, AgentRunPhase::Done).await);
    assert!(
        events.iter().any(|e| matches!(&e.kind,
        EventKind::Compaction(CompactionEvent::Compacted {reason:actual,..}) if *actual == reason))
    );
    let requests = chat_requests(&h);
    let current = result(requests.last().unwrap(), "current-plan");
    assert_eq!(current["revision"], before.revision);
    assert_eq!(
        current["nodes"],
        serde_json::to_value(&before.nodes).unwrap()
    );
    assert_eq!(before.nodes.is_empty(), cleared);
    if !cleared {
        assert_eq!(before.nodes[0].title, "Latest authoritative task");
    }
    if reason == CompactionReason::Agent {
        let messages = requests.last().unwrap()["messages"].as_array().unwrap();
        let compact = messages
            .iter()
            .position(|m| m["tool_call_id"] == "compact-plan")
            .unwrap();
        let read = messages
            .iter()
            .position(|m| m["tool_call_id"] == "current-plan")
            .unwrap();
        assert!(compact < read);
    }
    verify_trace(&h, run, &events, 1);
}

#[tokio::test]
async fn latest_plan_and_clear_survive_all_compaction_paths_then_reuse_the_wire_prefix() {
    for reason in [
        CompactionReason::Manual,
        CompactionReason::Automatic,
        CompactionReason::Agent,
    ] {
        for cleared in [false, true] {
            plan_compaction_reuses_new_prefix(reason, cleared).await;
        }
    }
}

#[tokio::test]
async fn plan_and_note_handoff_present_current_state_then_reuse_each_roots_wire_prefix() {
    let mut h = harness(
        vec![
            update(
                "handoff-plan",
                0,
                json!([add("Verify delegated findings", "in_progress")]),
            ),
            note_response("handoff-note", "Frozen handoff finding"),
            read_response(0),
            call(
                "handoff",
                "handoff_to_lane",
                json!({"original_request":"Inspect changes","escalation_reason":"Need coordinated verification"}),
            ),
            read("inherited-plan"),
            read_response(2),
            text_response("ready"),
        ],
        1_000_000,
    );
    let (_config, _db) = store(&mut h);
    let source = h
        .runtime
        .delegate_chat(
            "plan-handoff",
            Role::Worker,
            "Inspect changes".into(),
            RunConfig {
                conversation: true,
                ..Default::default()
            },
        )
        .unwrap();
    let mut events = Vec::new();
    let recipient = loop {
        let event = h.receiver.recv().await.unwrap();
        let recipient = match &event.kind {
            EventKind::Lifecycle(LifecycleEvent::EscalationRequested { new_run_id, .. }) => {
                Some(new_run_id.parse::<RunId>().unwrap())
            }
            _ => None,
        };
        events.push(event);
        if let Some(id) = recipient {
            break id;
        }
    };
    events.extend(through_phase(&mut h.receiver, recipient, AgentRunPhase::Done).await);
    assert_eq!(h.runtime.wait(source).await.unwrap(), AgentRunPhase::Done);
    let child = event_bus::escalation_thread_id(&recipient.to_string());
    assert_eq!(
        h.runtime
            .conversation_plan_summary("plan-handoff")
            .unwrap()
            .revision,
        0
    );
    let current = h
        .runtime
        .conversation_plan_tree(&child, None, None)
        .unwrap();
    assert_eq!(current.revision, 2);
    assert_eq!(
        current.nodes[0].status,
        Some(event_bus::conversation_plan::PlanTaskStatus::Interrupted)
    );
    let requests = chat_requests(&h);
    assert_eq!(requests.len(), 7);
    let inherited = result(&requests[5], "inherited-plan");
    assert_eq!(inherited["revision"], 1);
    assert_eq!(inherited["nodes"][0]["status"], "in_progress");
    assert_eq!(inherited["nodes"][0]["title"], current.nodes[0].title);
    let notes = wire_working_notes(&requests[4]);
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0]["body"], "Frozen handoff finding");
    assert_eq!(notes[0]["writer_run_id"], source.to_string());
    // Each root has its own affinity/cache. The handoff is not permission to
    // rewrite an already-sent prefix of either root.
    verify_root_prefix(&h, source, &events, &requests[..4]);
    verify_root_prefix(&h, recipient, &events, &requests[4..]);
    assert_eq!(h.mock.remaining_scripts(), 0);
}

#[tokio::test]
async fn saved_plan_switch_notice_appends_once_after_a_tool_round_and_reuses_the_prefix() {
    for planned in [false, true] {
        let mut script = Vec::new();
        if planned {
            script.push(update(
                "saved-plan",
                0,
                json!([add("Saved task", "in_progress")]),
            ));
        }
        script.extend([
            text_response("saved"),
            read("resume-read"),
            read_response(2),
            text_response("done"),
        ]);
        let mut h = harness(script, 1_000_000);
        let (config, db) = store(&mut h);
        let run = h
            .runtime
            .delegate_chat(
                "saved-plan",
                Role::Worker,
                "Original task".into(),
                RunConfig {
                    conversation: true,
                    interactive: true,
                    keep_alive: true,
                    ..Default::default()
                },
            )
            .unwrap();
        let mut events = through_phase(&mut h.receiver, run, AgentRunPhase::Waiting).await;
        h.runtime.stop(run, runtime::StopScope::SelfOnly).unwrap();
        events.extend(through_phase(&mut h.receiver, run, AgentRunPhase::Stopped).await);
        let before = chat_requests(&h);
        assert!(before.iter().all(|r| notices(r).is_empty()));
        age_record(&config, run);
        recompose_runtime(&mut h, &config, Some(&db));
        h.runtime
            .resume_chat(
                run,
                RunConfig {
                    conversation: true,
                    interactive: true,
                    keep_alive: true,
                    ..Default::default()
                },
            )
            .unwrap();
        events.extend(through_phase(&mut h.receiver, run, AgentRunPhase::Waiting).await);
        h.runtime
            .send_message(run, "Continue the saved task".into())
            .unwrap();
        events.extend(through_phase(&mut h.receiver, run, AgentRunPhase::Waiting).await);
        h.runtime.cancel(run).unwrap();
        h.runtime.wait(run).await.unwrap();
        let requests = chat_requests(&h);
        assert_eq!(
            requests.len(),
            before.len() + 3,
            "no request solely for the notice"
        );
        let told = &requests[before.len()];
        let notice = notices(told);
        assert_eq!(notice.len(), 1);
        assert_eq!(
            last_message_text(told),
            serde_json::to_string(&notice[0]).unwrap(),
            "notice is last, after all ToolResults"
        );
        assert_eq!(
            result(&requests[before.len() + 1], "resume-read")["revision"],
            if planned { 2 } else { 0 }
        );
        if planned {
            assert!(notice[0].contains("Plan revision 2:") && notice[0].contains("interrupted 1"));
        } else {
            assert!(notice[0].contains("No plan exists yet"));
        }
        assert_eq!(notices(requests.last().unwrap()), notice);
        verify_trace(&h, run, &events, 0);
    }
}

#[tokio::test]
async fn fork_and_rewind_seeds_append_one_unplanned_notice_without_editing_source_history() {
    let mut h = harness(
        vec![
            update("source-plan", 0, json!([add("Source task", "pending")])),
            text_response("source turn"),
            read("fork-read"),
            read_response(2),
            text_response("fork done"),
            read("rewind-read"),
            read_response(2),
            text_response("rewind done"),
        ],
        1_000_000,
    );
    let (_config, _db) = store(&mut h);
    let source = h
        .runtime
        .delegate_chat(
            "source-plan",
            Role::Worker,
            "Source input".into(),
            RunConfig {
                conversation: true,
                interactive: true,
                keep_alive: true,
                ..Default::default()
            },
        )
        .unwrap();
    let mut source_events = through_phase(&mut h.receiver, source, AgentRunPhase::Waiting).await;
    let boundary = loop {
        if let Some(count) = source_events.iter().find_map(|e| match &e.kind {
            EventKind::Lifecycle(LifecycleEvent::TurnCompleted {
                run_id,
                context_len,
            }) if run_id == &source.to_string() => Some(*context_len),
            _ => None,
        }) {
            break count;
        }
        source_events.push(h.receiver.recv().await.unwrap());
    };
    let before = h
        .runtime
        .conversation_plan_tree("source-plan", None, None)
        .unwrap();
    let original = chat_requests(&h).last().unwrap().clone();
    for (thread, call_id) in [("fork-plan", "fork-read"), ("rewind-plan", "rewind-read")] {
        let start = chat_requests(&h).len();
        let branch = h
            .runtime
            .delegate_chat_seeded(
                thread,
                Role::Worker,
                "Branch input".into(),
                RunConfig {
                    conversation: true,
                    ..Default::default()
                },
                Some(runtime::ChatForkSeed {
                    source_run_id: source.to_string(),
                    context_len: boundary,
                }),
            )
            .unwrap();
        let events = through_phase(&mut h.receiver, branch, AgentRunPhase::Done).await;
        let requests = chat_requests(&h);
        let branch_requests = &requests[start..];
        assert_eq!(branch_requests.len(), 3);
        let prefix = original["messages"].as_array().unwrap();
        assert_eq!(
            &branch_requests[0]["messages"].as_array().unwrap()[..prefix.len()],
            prefix
        );
        let notices = notices(&branch_requests[0]);
        assert_eq!(notices.len(), 1);
        assert!(notices[0].contains("No plan exists yet"));
        assert_eq!(
            last_message_text(&branch_requests[0]),
            serde_json::to_string(&notices[0]).unwrap()
        );
        assert_eq!(result(&branch_requests[1], call_id)["nodes"], json!([]));
        verify_root_prefix(&h, branch, &events, branch_requests);
        assert_eq!(
            h.runtime
                .conversation_plan_tree("source-plan", None, None)
                .unwrap(),
            before
        );
    }
    h.runtime.cancel(source).unwrap();
    h.runtime.wait(source).await.unwrap();
    assert_eq!(h.mock.remaining_scripts(), 0);
}
