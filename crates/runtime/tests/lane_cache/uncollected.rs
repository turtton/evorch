//! Suppression is new append-only tool output, never a cache boundary.
use super::*;

fn seed_lane(store: &storage::Storage, id: &str, at_ns: i64, terminal: bool) {
    let brief: event_bus::LaneBrief = serde_json::from_value(brief(id)).unwrap();
    let lane = store
        .handle()
        .create_lane(storage::lanes::NewLane {
            lane_id: id.into(),
            thread_id: format!("thread-{id}"),
            origin_thread_id: "uncollected-lead".into(),
            origin_run_id: "seed".into(),
            creation_key: id.into(),
            brief,
            start_seed: "seed".into(),
            project_id: None,
            project_root: None,
            title: None,
            at_ns,
        })
        .unwrap()
        .lane;
    if terminal {
        ready(store, &lane.lane_id, at_ns);
    }
}
fn ready(store: &storage::Storage, id: &str, at_ns: i64) {
    let lane = store.handle().lane(id).unwrap().unwrap();
    store
        .handle()
        .append_lane_report(
            id,
            lane.management_revision,
            event_bus::LaneReportInput {
                artifact_revision: None,
                failure_episode: None,
                status: event_bus::LaneStatus::Ready,
                summary: format!("artifact from {id}"),
                artifact_refs: vec![format!("artifact:{id}")],
                needs_human: false,
                question_refs: vec![],
                reason_code: None,
                producer: event_bus::LaneReportProducer::LaneAgent,
            },
            at_ns,
        )
        .unwrap();
}
async fn lead_call(
    h: &mut Harness,
    lead: RunId,
    id: &str,
    name: &str,
    args: Value,
    events: &mut Vec<Event>,
) -> Value {
    input(
        h,
        lead,
        "continue coordination",
        vec![call(id, name, args), text_response("continued")],
        events,
    )
    .await;
    result(h, id)
}

#[tokio::test]
async fn suppression_partial_collection_restore_and_release_preserve_wire_prefix() {
    let mut h = harness(vec![text_response("lead idle")], 1_000_000);
    let config = storage::StorageConfig {
        db_path: h._directory.path().join("uncollected.db"),
        ..Default::default()
    };
    let store = storage::Storage::open(config.clone()).unwrap();
    for (id, at, terminal) in [
        ("B", 1, true),
        ("C", 2, true),
        ("A", 3, true),
        ("long-running", 4, false),
    ] {
        seed_lane(&store, id, at, terminal);
    }
    h.runtime = h
        .runtime
        .with_run_store(runtime::RunStore::open(&config, store.handle()).unwrap());
    // A valid saved cohort near the boundary. The unit test exercises every
    // successful intent from 0 through 100, including duplicate intent retries.
    let credits = serde_json::from_value(json!({"spawned":[],"observed":[],"unobserved":20,
        "uncollected_lane_open_count":99,"uncollected_lane_ids":["B","C"]}))
    .unwrap();
    let lead = h
        .runtime
        .delegate_chat(
            "uncollected-lead",
            Role::Orchestrator,
            "coordinate".into(),
            RunConfig {
                interactive: true,
                keep_alive: true,
                conversation: true,
                spawn_credits: credits,
                ..Default::default()
            },
        )
        .unwrap();
    let mut events = through_phase(&mut h.receiver, lead, AgentRunPhase::Waiting).await;
    // long-running has no report; the second item is A. Neither can erase B/C.
    lead_call(
        &mut h,
        lead,
        "small-result",
        "lane_list",
        json!({"limit":2}),
        &mut events,
    )
    .await;
    let first = open(&mut h, lead, "hundredth-open", "boundary lane", &mut events).await;
    let opened = result(&h, "hundredth-open");
    let rejected = lead_call(
        &mut h,
        lead,
        "suppressed",
        "lane_open",
        json!({"brief":brief("blocked")}),
        &mut events,
    )
    .await;
    assert_eq!(rejected["code"], "lane_creation_suppressed");
    assert_eq!(rejected["uncollected_lane_open_count"], 100);
    assert_eq!(rejected["threshold"], 100);
    assert_eq!(rejected["lane_ids"], json!(["B", "C"]));
    // The exact creation envelope can be retried while new creation is suppressed.
    let retry = lead_call(
        &mut h,
        lead,
        "hundredth-open",
        "lane_open",
        json!({"brief":brief("boundary lane")}),
        &mut events,
    )
    .await;
    assert_eq!(retry["lane_id"], opened["lane_id"]);
    assert_eq!(retry["created"], false);
    // Existing lane work remains executable while the lead's new creation is held.
    input(
        &mut h,
        first,
        "continue existing work",
        vec![text_response("still working")],
        &mut events,
    )
    .await;
    h.runtime.stop(lead, runtime::StopScope::SelfOnly).unwrap();
    events.extend(through_phase(&mut h.receiver, lead, AgentRunPhase::Stopped).await);
    h.runtime.wait(lead).await.unwrap();
    h.runtime
        .resume_chat(
            lead,
            RunConfig {
                conversation: true,
                ..Default::default()
            },
        )
        .unwrap();
    events.extend(through_phase(&mut h.receiver, lead, AgentRunPhase::Waiting).await);
    let still = lead_call(
        &mut h,
        lead,
        "after-restore",
        "lane_open",
        json!({"brief":brief("still blocked")}),
        &mut events,
    )
    .await;
    assert_eq!(still["uncollected_lane_open_count"], 100);
    // New report sequence/summary alone cannot collect the old cohort.
    ready(&store, "B", i64::MAX - 2);
    let partial = lead_call(
        &mut h,
        lead,
        "collect-B",
        "lane_list",
        json!({"limit":1}),
        &mut events,
    )
    .await;
    assert_eq!(partial["items"][0]["lane_id"], "B");
    let remaining = lead_call(
        &mut h,
        lead,
        "C-still-blocks",
        "lane_open",
        json!({"brief":brief("C blocks")}),
        &mut events,
    )
    .await;
    assert_eq!(remaining["uncollected_lane_open_count"], 100);
    assert_eq!(remaining["lane_ids"], json!(["C"]));
    ready(&store, "C", i64::MAX - 1);
    lead_call(
        &mut h,
        lead,
        "collect-C",
        "lane_list",
        json!({"limit":1}),
        &mut events,
    )
    .await;
    open(&mut h, lead, "released-open", "released lane", &mut events).await;
    let requests: Vec<_> = h
        .mock
        .recorded_requests()
        .into_iter()
        .filter(|r| {
            r.path == Wire::ChatCompletions.path()
                && r.body["tools"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|tool| tool["function"]["name"] == "lane_open")
        })
        .map(|r| r.body)
        .collect();
    assert!(requests.len() > 10);
    for pair in requests.windows(2) {
        assert_append_only(CacheProtocol::OpenAi, &pair[0], &pair[1]).unwrap();
    }
    let usage: Vec<_> = events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::Provider(ProviderEvent::RequestCompleted {
                run_id: Some(id),
                input_tokens,
                cache_read_tokens,
                ..
            }) if id == &lead.to_string() => Some((*input_tokens, *cache_read_tokens)),
            EventKind::Diagnostic(d) if d.code == "CacheRegression" => panic!("{d:?}"),
            _ => None,
        })
        .collect();
    assert_eq!(usage.len(), requests.len());
    for pair in usage.windows(2) {
        assert!(
            pair[1].1 >= pair[0].0,
            "input-derived cache must retain the prior provider prefix"
        );
    }
    assert_eq!(h.mock.remaining_scripts(), 0);
}
