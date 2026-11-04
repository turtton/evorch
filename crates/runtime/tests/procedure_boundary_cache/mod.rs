//! A work period that ends with unfinished procedure items marks them
//! interrupted without a request of its own: the marked list reaches the model
//! only with the next request that human input causes, appended after
//! everything already sent. Tools, system and settings never depend on the list.
use super::*;

fn call(id: &str, name: &str, input: Value) -> ScriptedResponse {
    ScriptedResponse::tool_call(id, MODEL, 0, id, name, [input.to_string()])
}

fn chat_requests(h: &Harness) -> Vec<Value> {
    h.mock
        .recorded_requests()
        .into_iter()
        .filter(|request| request.path == Wire::ChatCompletions.path())
        .map(|request| request.body)
        .collect()
}

fn mentions(message: &Value, text: &str) -> bool {
    message.to_string().contains(text)
}

fn assert_stable_surface(requests: &[Value]) {
    for request in &requests[1..] {
        assert_eq!(request["tools"], requests[0]["tools"], "tool schemas/order");
        assert_eq!(request["model"], requests[0]["model"]);
        assert_eq!(request["messages"][0], requests[0]["messages"][0], "system");
    }
}

async fn waiting(h: &mut Harness, run: RunId, events: &mut Vec<Event>) {
    events.extend(through_phase(&mut h.receiver, run, AgentRunPhase::Waiting).await);
}

#[tokio::test]
async fn replace_advance_update_and_a_finished_turn_keep_the_wire_prefix() {
    let mut h = harness(
        vec![
            todo_response(
                "replace",
                json!([
                    {"content":"Inspect the implementation","status":"pending"},
                    {"content":"Verify the changes","status":"pending"}
                ]),
            ),
            call(
                "advance",
                "todo_write",
                json!({"advance":true,"expected_revision":1}),
            ),
            call(
                "update",
                "todo_write",
                json!({"update":{"index":1,"status":"in_progress"},"expected_revision":2}),
            ),
            read_response(0),
            text_response("The first turn ended with work still running"),
            text_response("The second turn"),
        ],
        1_000_000,
    );
    let run = h
        .runtime
        .delegate_chat(
            "procedure-boundary",
            Role::Worker,
            "Inspect and verify the requested changes".into(),
            RunConfig {
                conversation: true,
                interactive: true,
                keep_alive: true,
                ..Default::default()
            },
        )
        .unwrap();
    let review_storage = support::ReviewStorage::new();
    let mut events = Vec::new();
    waiting(&mut h, run, &mut events).await;
    review_storage.attach(&h.runtime);
    // Both running items were marked, and marking asked the provider nothing.
    let marked = h.runtime.thread_todo("procedure-boundary").unwrap();
    assert_eq!(marked.revision, 4);
    assert!(
        marked
            .items
            .iter()
            .all(|item| item.status == event_bus::ThreadTodoStatus::Interrupted)
    );
    assert_eq!(chat_requests(&h).len(), 5);

    h.runtime
        .send_message(run, "Continue after the interruption".into())
        .unwrap();
    waiting(&mut h, run, &mut events).await;
    h.runtime.stop(run, runtime::StopScope::SelfOnly).unwrap();
    events.extend(through_phase(&mut h.receiver, run, AgentRunPhase::Stopped).await);
    assert_eq!(
        h.runtime.thread_todo("procedure-boundary"),
        Some(marked.clone())
    );

    let requests = chat_requests(&h);
    assert_eq!(requests.len(), 6);
    let sent = wire_todo_snapshots(&requests[5]);
    assert_eq!(sent.len(), 1, "marked state is presented once");
    assert_eq!(sent[0].1, marked);
    let human = requests[5]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .position(|message| mentions(message, "Continue after the interruption"))
        .unwrap();
    assert!(sent[0].0 > human, "appended after the new input");
    // Earlier requests never carried the marked list: nothing was rewritten.
    assert!(requests[..5].iter().all(|request| {
        wire_todo_snapshots(request)
            .iter()
            .all(|(_, s)| s.revision < 4)
    }));
    assert_stable_surface(&requests);
    assert_append_only(CacheProtocol::OpenAi, &requests[4], &requests[5]).unwrap();
    verify_trace(&h, run, &events, 0);
}

#[tokio::test]
async fn a_marked_list_survives_manual_compaction_and_is_presented_once() {
    let review_storage = support::ReviewStorage::new();
    let old_reply = "old analysis that can be summarized ".repeat(100);
    let mut h = harness(
        vec![
            todo_response(
                "running",
                json!([{"content":"Active procedure step","status":"in_progress"}]),
            ),
            text_response(&old_reply),
            read_response(0),
            read_response(2),
            text_response("done"),
        ],
        1_000_000,
    );
    let run = h
        .runtime
        .delegate_chat(
            "marked-compaction",
            Role::Worker,
            "Inspect the requested changes".into(),
            RunConfig {
                conversation: true,
                interactive: true,
                ..Default::default()
            },
        )
        .unwrap();
    let mut events = Vec::new();
    waiting(&mut h, run, &mut events).await;
    let marked = h.runtime.thread_todo("marked-compaction").unwrap();
    assert_eq!(
        marked.items[0].status,
        event_bus::ThreadTodoStatus::Interrupted
    );
    h.runtime.compact(run).unwrap();
    review_storage.attach(&h.runtime);
    h.runtime
        .send_message(run, "Continue after the context boundary".into())
        .unwrap();
    events.extend(through_phase(&mut h.receiver, run, AgentRunPhase::Done).await);
    assert_eq!(
        h.runtime.thread_todo("marked-compaction"),
        Some(marked.clone())
    );
    let requests = chat_requests(&h);
    let snapshots = wire_todo_snapshots(requests.last().unwrap());
    assert_eq!(snapshots.len(), 1);
    assert_eq!(snapshots[0].1, marked);
    assert_stable_surface(&requests);
    verify_trace(&h, run, &events, 1);
}

fn lane_brief(objective: &str) -> Value {
    json!({"objective":objective,"inputs":[],"deliverables":["artifact"],"acceptance_criteria":["independent review"],"constraints":[],"workspace":{},"report_policy":"attached"})
}

/// The thread id the model was told for the lane it opened.
fn opened_thread(h: &Harness, id: &str) -> String {
    chat_requests(h)
        .iter()
        .rev()
        .find_map(|request| {
            request["messages"].as_array()?.iter().find_map(|message| {
                (message["role"] == "tool" && message["tool_call_id"] == id).then(|| {
                    let opened: Value =
                        serde_json::from_str(message["content"].as_str().unwrap()).unwrap();
                    opened["thread_id"].as_str().unwrap().to_owned()
                })
            })
        })
        .expect("lane_open result")
}

async fn send(h: &mut Harness, run: RunId, text: &str, script: Vec<ScriptedResponse>) {
    h.mock.enqueue(script);
    h.runtime.send_message(run, text.into()).unwrap();
    through_phase(&mut h.receiver, run, AgentRunPhase::Waiting).await;
}

#[tokio::test]
async fn a_lead_and_its_lane_each_present_only_their_own_marked_list() {
    let mut h = harness(vec![text_response("lead idle")], 1_000_000);
    let config = storage::StorageConfig {
        db_path: h._directory.path().join("procedure-lanes.db"),
        ..Default::default()
    };
    let db = storage::Storage::open(config.clone()).unwrap();
    h.runtime = h
        .runtime
        .with_run_store(runtime::RunStore::open(&config, db.handle()).unwrap());
    let lead = h
        .runtime
        .delegate_chat(
            "procedure-lead",
            Role::Orchestrator,
            "lead task".into(),
            RunConfig {
                interactive: true,
                keep_alive: true,
                conversation: true,
                ..Default::default()
            },
        )
        .unwrap();
    through_phase(&mut h.receiver, lead, AgentRunPhase::Waiting).await;
    h.mock.enqueue(vec![
        call(
            "open-lane",
            "lane_open",
            json!({"brief":lane_brief("procedure lane")}),
        ),
        text_response("lead idle"),
        text_response("lane idle"),
    ]);
    h.runtime
        .send_message(lead, "open an independent lane".into())
        .unwrap();
    let mut opened = through_phase(&mut h.receiver, lead, AgentRunPhase::Waiting).await;
    let lane = h
        .runtime
        .list_agents()
        .into_iter()
        .find(|row| row.parent_run_id.is_none() && row.run_id != lead)
        .expect("the lane root")
        .run_id;
    // The lane may have settled before the lead did.
    let lane_waiting = |event: &Event| {
        matches!(&event.kind, EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
            run_id, to: AgentRunPhase::Waiting, ..
        }) if run_id == &lane.to_string())
    };
    if !opened.iter().any(lane_waiting) {
        opened.extend(through_phase(&mut h.receiver, lane, AgentRunPhase::Waiting).await);
    }

    send(
        &mut h,
        lead,
        "plan the lead's work",
        vec![
            todo_response(
                "lead-plan",
                json!([{"content":"Lead coordination step","status":"in_progress"}]),
            ),
            text_response("lead planned"),
        ],
    )
    .await;
    send(
        &mut h,
        lane,
        "plan the lane's work",
        vec![
            todo_response(
                "lane-plan",
                json!([{"content":"Lane delivery step","status":"in_progress"}]),
            ),
            text_response("lane planned"),
        ],
    )
    .await;
    // Marking one root's list changed nothing of the other's.
    let lead_list = h.runtime.thread_todo("procedure-lead").unwrap();
    let lane_thread = opened_thread(&h, "open-lane");
    let lane_list = h.runtime.thread_todo(&lane_thread).unwrap();
    for list in [&lead_list, &lane_list] {
        assert_eq!(list.revision, 2);
        assert_eq!(
            list.items[0].status,
            event_bus::ThreadTodoStatus::Interrupted
        );
    }
    assert_ne!(lead_list.list_id, lane_list.list_id);
    let before = chat_requests(&h).len();
    send(
        &mut h,
        lead,
        "next lead input",
        vec![text_response("lead next")],
    )
    .await;
    send(
        &mut h,
        lane,
        "next lane input",
        vec![text_response("lane next")],
    )
    .await;
    assert_eq!(chat_requests(&h).len(), before + 2);

    let requests = chat_requests(&h);
    let is_lead = |request: &Value| {
        request["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["function"]["name"] == "lane_open")
    };
    let lead_requests: Vec<_> = requests.iter().filter(|r| is_lead(r)).cloned().collect();
    let lane_requests: Vec<_> = requests.iter().filter(|r| !is_lead(r)).cloned().collect();
    for (group, expected) in [(&lead_requests, &lead_list), (&lane_requests, &lane_list)] {
        let last = wire_todo_snapshots(group.last().unwrap());
        assert_eq!(last.len(), 1, "only this root's list is presented");
        assert_eq!(&last[0].1, expected);
        for request in group.iter() {
            assert!(
                wire_todo_snapshots(request)
                    .iter()
                    .all(|(_, snapshot)| snapshot.thread_id == expected.thread_id),
                "another thread's list leaked into this root's request"
            );
        }
        for pair in group.windows(2) {
            assert_append_only(CacheProtocol::OpenAi, &pair[0], &pair[1]).unwrap();
        }
        assert_stable_surface(group);
    }
    for id in [lead, lane] {
        h.runtime.cancel(id).unwrap();
        h.runtime.wait(id).await.unwrap();
    }
    assert_eq!(h.mock.remaining_scripts(), 0);
}
