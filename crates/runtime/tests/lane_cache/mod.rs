use super::*;
mod delivery;
mod forwarding;
mod questions;
mod verification;
fn call(id: &str, name: &str, input: Value) -> ScriptedResponse {
    ScriptedResponse::tool_call(id, MODEL, 0, id, name, [input.to_string()])
}
fn brief(objective: &str) -> Value {
    json!({"objective":objective,"inputs":[],"deliverables":["artifact"],"acceptance_criteria":["independent review"],"constraints":[],"workspace":{},"report_policy":"attached"})
}
fn result(h: &Harness, id: &str) -> Value {
    h.mock
        .recorded_requests()
        .iter()
        .rev()
        .filter(|r| r.path == Wire::ChatCompletions.path())
        .find_map(|r| {
            r.body["messages"].as_array()?.iter().rev().find_map(|m| {
                (m["role"] == "tool" && m["tool_call_id"] == id)
                    .then(|| serde_json::from_str(m["content"].as_str().unwrap()).unwrap())
            })
        })
        .unwrap_or_else(|| panic!("missing tool result {id}"))
}
async fn input(
    h: &mut Harness,
    run: RunId,
    text: &str,
    scripts: Vec<ScriptedResponse>,
    events: &mut Vec<Event>,
) {
    h.mock.enqueue(scripts);
    h.runtime.send_message(run, text.into()).unwrap();
    events.extend(through_phase(&mut h.receiver, run, AgentRunPhase::Waiting).await);
}
async fn open(
    h: &mut Harness,
    lead: RunId,
    id: &str,
    objective: &str,
    events: &mut Vec<Event>,
) -> RunId {
    let start = events.len();
    input(
        h,
        lead,
        "open an independent lane",
        vec![
            call(id, "lane_open", json!({"brief":brief(objective)})),
            text_response("idle"),
            text_response("idle"),
        ],
        events,
    )
    .await;
    let opened = result(h, id);
    let lane: RunId = opened["root_run_id"].as_str().unwrap().parse().unwrap();
    assert!(opened["created"].as_bool().unwrap());
    if !events[start..].iter().any(|e| matches!(&e.kind,EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {run_id,to:AgentRunPhase::Waiting,..}) if run_id == &lane.to_string())) {
        events.extend(through_phase(&mut h.receiver,lane,AgentRunPhase::Waiting).await);
    }
    lane
}
#[tokio::test]
async fn lane_tools_pages_reports_and_restores_preserve_each_wire_prefix() {
    let mut h = harness(vec![text_response("lead idle")], 1_000_000);
    let config = storage::StorageConfig {
        db_path: h._directory.path().join("lanes.db"),
        ..Default::default()
    };
    let db = storage::Storage::open(config.clone()).unwrap();
    h.runtime = h
        .runtime
        .with_run_store(runtime::RunStore::open(&config, db.handle()).unwrap());
    let lead = h
        .runtime
        .delegate_chat(
            "lane-lead",
            Role::Orchestrator,
            "lane lead task".into(),
            RunConfig {
                interactive: true,
                keep_alive: true,
                conversation: true,
                ..Default::default()
            },
        )
        .unwrap();
    let mut events = through_phase(&mut h.receiver, lead, AgentRunPhase::Waiting).await;
    let lane = open(&mut h, lead, "open-first", "first lane", &mut events).await;
    let second = open(&mut h, lead, "open-second", "second lane", &mut events).await;
    input(&mut h,lane,"report milestone",vec![call("report-one","lane_report",json!({"status":"ready","summary":"first revision","artifact_refs":["artifact:one"]})),text_response("reported")],&mut events).await;
    input(
        &mut h,
        lead,
        "read page one",
        vec![
            call("page-one", "lane_list", json!({"limit":1})),
            text_response("page one read"),
        ],
        &mut events,
    )
    .await;
    let page_one = result(&h, "page-one");
    assert_eq!(page_one["items"].as_array().unwrap().len(), 1);
    assert_eq!(page_one["has_more"], true);
    let cursor = page_one["next_cursor"].as_str().unwrap();
    input(&mut h,lane,"new report revision",vec![call("report-two","lane_report",json!({"status":"failed","summary":"updated revision","artifact_refs":["artifact:two"]})),text_response("updated")],&mut events).await;
    input(
        &mut h,
        lead,
        "read the next page",
        vec![
            call("page-two", "lane_list", json!({"limit":1,"cursor":cursor})),
            text_response("page two read"),
        ],
        &mut events,
    )
    .await;
    let page_two = result(&h, "page-two");
    assert_eq!(page_two["has_more"], false);
    assert_eq!(page_one["snapshot_revision"], page_two["snapshot_revision"]);
    assert_ne!(
        page_one["items"][0]["lane_id"],
        page_two["items"][0]["lane_id"]
    );
    input(
        &mut h,
        lead,
        "reread current reports",
        vec![
            call("page-new", "lane_list", json!({"limit":20})),
            text_response("reports read"),
        ],
        &mut events,
    )
    .await;
    let fresh = result(&h, "page-new");
    assert!(
        fresh["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["latest_report"]["summary"] == "updated revision")
    );
    for id in [lead, lane] {
        h.runtime.stop(id, runtime::StopScope::SelfOnly).unwrap();
        events.extend(through_phase(&mut h.receiver, id, AgentRunPhase::Stopped).await);
        h.runtime.wait(id).await.unwrap();
        h.runtime
            .resume_chat(
                id,
                RunConfig {
                    conversation: true,
                    ..Default::default()
                },
            )
            .unwrap();
        events.extend(through_phase(&mut h.receiver, id, AgentRunPhase::Waiting).await);
        input(
            &mut h,
            id,
            "a normal followup after restore",
            vec![text_response("restored answer")],
            &mut events,
        )
        .await;
    }
    for id in [lead, lane, second] {
        h.runtime.cancel(id).unwrap();
        h.runtime.wait(id).await.unwrap();
    }
    assert_eq!(h.mock.remaining_scripts(), 0);
    let requests: Vec<_> = h
        .mock
        .recorded_requests()
        .into_iter()
        .filter(|r| r.path == Wire::ChatCompletions.path())
        .map(|r| r.body)
        .collect();
    let lead_requests: Vec<_> = requests
        .iter()
        .filter(|r| {
            r["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|t| t["function"]["name"] == "lane_open")
        })
        .cloned()
        .collect();
    let first_lane: Vec<_> = requests
        .iter()
        .filter(|r| {
            !r["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|t| t["function"]["name"] == "lane_open")
                && r["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|m| m["role"] == "user")
                    .unwrap()["content"]
                    .to_string()
                    .contains("first lane")
        })
        .cloned()
        .collect();
    assert!(first_lane.len() >= 6);
    for request in &first_lane {
        let tools = request["tools"].as_array().unwrap();
        assert!(tools.iter().any(|t| t["function"]["name"] == "lane_report"));
        assert!(!tools.iter().any(|t| t["function"]["name"] == "lane_open"));
        let first_user = request["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["role"] == "user")
            .unwrap();
        assert!(
            first_user["content"]
                .to_string()
                .contains("Agent-authored lane brief")
        );
    }
    // Independent byte-prefix assertions and input-derived mock cache accounting.
    for group in [&lead_requests, &first_lane] {
        for pair in group.windows(2) {
            assert_append_only(CacheProtocol::OpenAi, &pair[0], &pair[1]).unwrap();
        }
    }
    for (run, group) in [(lead, &lead_requests), (lane, &first_lane)] {
        let usage: Vec<_> = events
            .iter()
            .filter_map(|event| match &event.kind {
                EventKind::Provider(ProviderEvent::RequestCompleted {
                    run_id: Some(id),
                    input_tokens,
                    cache_read_tokens,
                    ..
                }) if id == &run.to_string() => Some((*input_tokens, *cache_read_tokens)),
                EventKind::Diagnostic(diagnostic) if diagnostic.code == "CacheRegression" => {
                    panic!("{diagnostic:?}")
                }
                _ => None,
            })
            .collect();
        assert_eq!(usage.len(), group.len());
        // A distinct lane may reuse the catalog prefix already sent by the lead.
        // Only the fixture's very first root request must be cold.
        if run == lead {
            assert_eq!(usage[0].1, 0);
        }
        assert!(usage[0].1 <= usage[0].0);
        for pair in usage.windows(2) {
            assert!(pair[0].0 > 0);
            assert!(pair[1].1 >= pair[0].0);
        }
    }
    // Old tool results stay immutable after report updates, pagination and restore.
    assert_eq!(result(&h, "page-one"), page_one);
}
