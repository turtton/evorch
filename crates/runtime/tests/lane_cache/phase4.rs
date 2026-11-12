//! Phase 4E: a suppressed creation, supplements, brief revisions and lead closes are all
//! new append-only history. Held, released and re-held creation never rewrite what was
//! sent, and the toolset stays the same throughout.
use super::*;

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

async fn lead_turn(
    h: &mut Harness,
    lead: RunId,
    mut calls: Vec<ScriptedResponse>,
    events: &mut Vec<Event>,
) {
    calls.push(text_response("continued"));
    input(h, lead, "continue coordination", calls, events).await;
}

fn instruction(
    store: &storage::Storage,
    id: &str,
    name: &str,
    key: &str,
    extra: Value,
) -> ScriptedResponse {
    let lane = store.handle().lane(id).unwrap().unwrap();
    let mut args = json!({
        "operation_id": key,
        "lane_id": id,
        "expected_management_revision": lane.management_revision,
        "expected_brief_revision": lane.brief_revision,
    });
    args.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    call(key, name, args)
}

fn close(store: &storage::Storage, id: &str, key: &str) -> ScriptedResponse {
    let context = store.handle().lane_close_context(id.into()).unwrap();
    call(
        key,
        "lane_close",
        json!({
            "lane_id": id,
            "ready_seq": context.latest_ready_seq.unwrap(),
            "expected_management_revision": context.management_revision,
            "expected_request_revision": context.request_revision,
            "note": "judged done",
        }),
    )
}

#[tokio::test]
async fn hold_supplement_revision_close_and_release_preserve_wire_prefix() {
    let mut h = harness(vec![text_response("lead idle")], 1_000_000);
    let config = storage::StorageConfig {
        db_path: h._directory.path().join("phase4.db"),
        ..Default::default()
    };
    let store = storage::Storage::open(config.clone()).unwrap();
    // The threshold is not what is under test (uncollected.rs runs the default 100):
    // a small one keeps the history short.
    h.runtime = h
        .runtime
        .clone()
        .with_uncollected_lane_open_limit(2)
        .with_run_store(runtime::RunStore::open(&config, store.handle()).unwrap());
    let lead = h
        .runtime
        .delegate_chat(
            "phase4-lead",
            Role::Orchestrator,
            "coordinate lanes".into(),
            RunConfig {
                interactive: true,
                keep_alive: true,
                conversation: true,
                ..Default::default()
            },
        )
        .unwrap();
    let mut events = through_phase(&mut h.receiver, lead, AgentRunPhase::Waiting).await;
    let mut lane_ids = Vec::new();
    for n in 0..2 {
        open(
            &mut h,
            lead,
            &format!("open-{n}"),
            &format!("lane {n}"),
            &mut events,
        )
        .await;
        lane_ids.push(
            result(&h, &format!("open-{n}"))["lane_id"]
                .as_str()
                .unwrap()
                .to_owned(),
        );
    }
    let (l0, l1) = (lane_ids[0].clone(), lane_ids[1].clone());
    let mut at = i64::MAX / 2;
    for lane in [&l0, &l1] {
        at += 1;
        ready(&store, lane, at);
    }
    for n in 2..4 {
        open(
            &mut h,
            lead,
            &format!("open-{n}"),
            &format!("lane {n}"),
            &mut events,
        )
        .await;
    }
    let refused = |id: &str| call(id, "lane_open", json!({"brief":brief("refused")}));

    // Held: the supplement keeps the Ready current, so nothing changes for creation.
    let supplement = instruction(
        &store,
        &l0,
        "lane_send",
        "send-0",
        json!({"message":"prefer the small fix"}),
    );
    lead_turn(
        &mut h,
        lead,
        vec![refused("refused-1"), supplement],
        &mut events,
    )
    .await;
    assert_eq!(result(&h, "refused-1")["code"], "lane_creation_suppressed");
    assert_eq!(result(&h, "refused-1")["uncollected_lane_open_count"], 2);
    assert_eq!(result(&h, "send-0")["status"], "accepted");
    lead_turn(&mut h, lead, vec![refused("refused-2")], &mut events).await;
    assert_eq!(result(&h, "refused-2")["code"], "lane_creation_suppressed");

    // Revised: both Ready reports are stale, nothing finished waits, creation works.
    let revise_0 = instruction(
        &store,
        &l0,
        "lane_revise_brief",
        "revise-0",
        json!({"brief":brief("reworked 0")}),
    );
    let revise_1 = instruction(
        &store,
        &l1,
        "lane_revise_brief",
        "revise-1",
        json!({"brief":brief("reworked 1")}),
    );
    lead_turn(&mut h, lead, vec![revise_0, revise_1], &mut events).await;
    assert_eq!(result(&h, "revise-0")["operation"]["brief_revision"], 2);
    assert_eq!(result(&h, "revise-1")["operation"]["brief_revision"], 2);
    open(
        &mut h,
        lead,
        "open-reworking",
        "lane while reworking",
        &mut events,
    )
    .await;

    // A lane that finished unrelated to the cohort: a supplement leaves its Ready valid and
    // the lead closes it without a review. Creation keeps working throughout.
    let done = result(&h, "open-2")["lane_id"].as_str().unwrap().to_owned();
    at += 1;
    ready(&store, &done, at);
    let send = instruction(
        &store,
        &done,
        "lane_send",
        "send-done",
        json!({"message":"last check before closing"}),
    );
    lead_turn(&mut h, lead, vec![send], &mut events).await;
    let close_done = close(&store, &done, "close-done");
    lead_turn(&mut h, lead, vec![close_done], &mut events).await;
    assert_eq!(result(&h, "send-done")["status"], "accepted");
    assert_eq!(result(&h, "close-done")["outcome"], "closed");

    // A read, then another creation: more history, nothing rewritten.
    lead_turn(
        &mut h,
        lead,
        vec![call("read", "lane_list", json!({}))],
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
                    .any(|tool| tool["function"]["name"] == "lane_close")
        })
        .map(|r| r.body)
        .collect();
    assert!(requests.len() > 10, "{}", requests.len());
    for pair in requests.windows(2) {
        assert_append_only(CacheProtocol::OpenAi, &pair[0], &pair[1]).unwrap();
    }
    assert_eq!(
        requests[0]["tools"],
        requests.last().unwrap()["tools"],
        "the toolset is stable through hold, revision, close and release"
    );
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
