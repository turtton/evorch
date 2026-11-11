//! A lead close without any review, and a human end of the ledger entry, keep every
//! already-sent prefix and the toolset, and the next turn reuses them. Input-derived
//! cache usage and independent wire-prefix assertions, as for the other lane paths.
use super::*;

/// The lead's requests that carry the lane toolset, in order.
fn lead_requests(h: &Harness) -> Vec<Value> {
    h.mock
        .recorded_requests()
        .into_iter()
        .filter(|r| r.path == Wire::ChatCompletions.path())
        .map(|r| r.body)
        .filter(|r| {
            r["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|t| t["function"]["name"] == "lane_close")
        })
        .collect()
}
fn assert_prefix_preserved(requests: &[Value]) {
    assert!(requests.len() > 5, "{}", requests.len());
    for pair in requests.windows(2) {
        assert_append_only(CacheProtocol::OpenAi, &pair[0], &pair[1]).unwrap();
    }
    assert_eq!(
        requests[0]["tools"],
        requests.last().unwrap()["tools"],
        "the toolset is stable across the close"
    );
}
async fn ready_lane(
    h: &mut Harness,
    events: &mut Vec<Event>,
) -> (RunId, RunId, String, storage::Storage) {
    let config = storage::StorageConfig {
        db_path: h._directory.path().join("close.db"),
        ..Default::default()
    };
    let db = storage::Storage::open(config.clone()).unwrap();
    h.runtime = h
        .runtime
        .clone()
        .with_run_store(runtime::RunStore::open(&config, db.handle()).unwrap());
    let lead = h
        .runtime
        .delegate_chat(
            "lead",
            Role::Orchestrator,
            "close lanes on your own judgement".into(),
            RunConfig {
                interactive: true,
                keep_alive: true,
                conversation: true,
                ..Default::default()
            },
        )
        .unwrap();
    events.extend(through_phase(&mut h.receiver, lead, AgentRunPhase::Waiting).await);
    let lane = open(h, lead, "open-close", "a lane to close", events).await;
    let lane_id = result(h, "open-close")["lane_id"]
        .as_str()
        .unwrap()
        .to_owned();
    // No pinned artifact and no review: nothing but the lane's own claim.
    input(
        h,
        lane,
        "report ready",
        vec![
            call(
                "ready",
                "lane_report",
                json!({"status":"ready","summary":"claimed ready, nothing pinned"}),
            ),
            text_response("reported"),
        ],
        events,
    )
    .await;
    (lead, lane, lane_id, db)
}

#[tokio::test]
async fn a_close_without_a_review_keeps_the_prefix_and_a_stale_rejection_names_the_current_revisions()
 {
    let mut h = harness(vec![text_response("lead idle")], 1_000_000);
    let mut events = Vec::new();
    let (lead, lane, lane_id, _db) = ready_lane(&mut h, &mut events).await;
    // The lead does not know the revisions yet: the rejection says what they are now.
    input(
        &mut h,
        lead,
        "close what I remember",
        vec![
            call(
                "close-blind",
                "lane_close",
                json!({"lane_id":lane_id,"ready_seq":1,"expected_management_revision":0,"expected_request_revision":0,"note":"done"}),
            ),
            text_response("saw the current revisions"),
        ],
        &mut events,
    )
    .await;
    let rejected = result(&h, "close-blind");
    assert_eq!(rejected["error"]["code"], "stale_management");
    let message = rejected["error"]["message"].as_str().unwrap();
    let current: Value = serde_json::from_str(message.split("current: ").nth(1).unwrap()).unwrap();
    input(
        &mut h,
        lead,
        "close with the current revisions",
        vec![
            call(
                "close-now",
                "lane_close",
                json!({
                    "lane_id":lane_id,"ready_seq":1,
                    "expected_management_revision":current["management_revision"],
                    "expected_request_revision":current["request_revision"],
                    "note":"accepted without a review"
                }),
            ),
            text_response("closed"),
        ],
        &mut events,
    )
    .await;
    let closed = result(&h, "close-now");
    assert_eq!(closed["outcome"], "closed", "{closed}");
    assert_eq!(closed["kind"], "lead_closed");
    assert!(closed["review"].is_null());
    input(
        &mut h,
        lead,
        "continue after the close",
        vec![text_response(
            "prefix reusable after a close without review",
        )],
        &mut events,
    )
    .await;
    for run in [lead, lane] {
        h.runtime.cancel(run).unwrap();
        h.runtime.wait(run).await.unwrap();
    }
    assert_eq!(h.mock.remaining_scripts(), 0);
    let requests = lead_requests(&h);
    assert_prefix_preserved(&requests);
    assert!(
        requests.last().unwrap()["messages"]
            .to_string()
            .contains("accepted without a review")
            || requests.last().unwrap()["messages"]
                .to_string()
                .contains("close-now"),
        "the earlier close result stays in the history unchanged"
    );
}

#[tokio::test]
async fn a_human_end_of_the_ledger_entry_keeps_the_prefix_for_the_next_turn() {
    let mut h = harness(vec![text_response("lead idle")], 1_000_000);
    let mut events = Vec::new();
    let (lead, lane, lane_id, db) = ready_lane(&mut h, &mut events).await;
    input(
        &mut h,
        lead,
        "look at the lane list",
        vec![
            call("list-before", "lane_list", json!({"limit":20})),
            text_response("listed"),
        ],
        &mut events,
    )
    .await;
    // A human discards the lane from the host path; nothing is stopped or detached.
    let now = db.handle().lane_close_context(lane_id.clone()).unwrap();
    let discarded = h
        .runtime
        .close_lane_from_host(
            runtime::lane_management::LaneHumanContext,
            runtime::lane_close::HumanLaneCloseRequest {
                lane_id: lane_id.clone(),
                ready_seq: now.latest_ready_seq,
                expected_management_revision: now.management_revision,
                expected_request_revision: now.request_revision,
                discard: true,
                note: "no longer needed".into(),
                surface: "web".into(),
                client_id: Some("client-1".into()),
            },
            &[],
        )
        .unwrap();
    assert_eq!(discarded.kind, event_bus::lane::LaneCloseKind::Discarded);
    // The lead tries to close afterwards: the human's record stands.
    input(
        &mut h,
        lead,
        "close it as well",
        vec![
            call(
                "close-after",
                "lane_close",
                json!({
                    "lane_id":lane_id,"ready_seq":1,
                    "expected_management_revision":now.management_revision,
                    "expected_request_revision":now.request_revision,
                    "note":"also closing"
                }),
            ),
            text_response("the human ended it"),
        ],
        &mut events,
    )
    .await;
    let after = result(&h, "close-after");
    assert_eq!(after["outcome"], "already_closed", "{after}");
    assert_eq!(after["kind"], "discarded");
    input(
        &mut h,
        lead,
        "continue after the human close",
        vec![text_response("prefix reusable after a human close")],
        &mut events,
    )
    .await;
    for run in [lead, lane] {
        h.runtime.cancel(run).unwrap();
        h.runtime.wait(run).await.unwrap();
    }
    assert_eq!(h.mock.remaining_scripts(), 0);
    assert_prefix_preserved(&lead_requests(&h));
}

/// The lead's own requests: the lane toolset is there, the Reviewer's prompt is not.
fn lead_only_requests(h: &Harness) -> Vec<Value> {
    lead_requests(h)
        .into_iter()
        .filter(|r| !r["messages"].to_string().contains("OBSERVATION LIMITS"))
        .collect()
}
/// A tool result already sent stays byte-identical in every later request.
fn assert_result_unchanged(requests: &[Value], call_id: &str, first: &Value) {
    let mut seen = 0;
    for request in requests {
        for message in request["messages"].as_array().unwrap() {
            if message["tool_call_id"] == call_id {
                seen += 1;
                let again: Value =
                    serde_json::from_str(message["content"].as_str().unwrap()).unwrap();
                assert_eq!(&again, first, "{call_id} was rewritten");
            }
        }
    }
    assert!(seen > 1, "{call_id} is not resent in later requests");
}

/// A close that attaches a Reviewer record whose observation limits were recorded
/// (the Ready pinned no artifact) is the lead's decision as usual. Neither the next
/// turn nor a Stop and restore of the lead rewrites what was sent, the close stays
/// recorded, and a replay of it after the restore is the same close.
#[tokio::test]
async fn a_close_attaching_a_review_with_observation_limits_keeps_the_prefix_through_the_next_turn_and_a_restore()
 {
    let (tx, mut reads) = tokio::sync::mpsc::unbounded_channel();
    let mut h = harness_with_read(
        vec![text_response("lead idle")],
        1_000_000,
        Arc::new(super::verification::ReadGate(tx)),
    );
    let mut events = Vec::new();
    let (lead, lane, lane_id, _db) = ready_lane(&mut h, &mut events).await;
    h.mock.enqueue([
        call(
            "verify-limited",
            "lane_verify",
            json!({"lane_id":lane_id,"ready_seq":1}),
        ),
        call("read-boundary", "read", json!({"index":0})),
        call("read-boundary", "read", json!({"index":0})),
    ]);
    h.runtime
        .send_message(lead, "review what the lane claims".into())
        .unwrap();
    // Both next provider requests have been recorded before either read can finish.
    let a = reads.recv().await.unwrap();
    let b = reads.recv().await.unwrap();
    let (lead_gate, reviewer_gate) = if a.0 == lead.to_string() {
        (a.1, b)
    } else {
        (b.1, a)
    };
    let verification = result(&h, "verify-limited");
    let reference = verification["verification_ref"]
        .as_str()
        .unwrap()
        .to_owned();
    let reviewer: RunId = verification["reviewer_run_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(reviewer_gate.0, reviewer.to_string());
    let limits = verification["caveats"].clone();
    assert_eq!(
        limits.as_array().unwrap().len(),
        1,
        "the unpinned artifact is recorded as an observation limit: {verification}"
    );
    h.mock
        .enqueue([text_response("Reviewer started; awaiting result")]);
    lead_gate.send(()).unwrap();
    events.extend(through_phase(&mut h.receiver, lead, AgentRunPhase::Waiting).await);
    h.mock.enqueue([
        call(
            "submit",
            "submit_lane_verification",
            json!({
                "verification_ref":reference,"verdict":"rejected",
                "checks":[{"criterion_index":0,"passed":false,"evidence_refs":[]}],
                "evidence_refs":[],
                "findings":["no artifact could be observed"]
            }),
        ),
        text_response("Reviewer result observed"),
    ]);
    reviewer_gate.1.send(()).unwrap();
    h.runtime.wait(reviewer).await.unwrap();
    events.extend(through_phase(&mut h.receiver, lead, AgentRunPhase::Waiting).await);

    let close_args = json!({
        "lane_id":lane_id,"ready_seq":1,
        "expected_management_revision":verification["management_revision"],
        "expected_request_revision":verification["request_revision"],
        "note":"closing despite the observation limits",
        "verification_ref":reference
    });
    input(
        &mut h,
        lead,
        "close with the limited review attached",
        vec![
            call("close-limited", "lane_close", close_args.clone()),
            text_response("closed with the limits in view"),
        ],
        &mut events,
    )
    .await;
    let closed = result(&h, "close-limited");
    assert_eq!(closed["outcome"], "closed", "{closed}");
    assert_eq!(closed["kind"], "lead_closed");
    assert_eq!(closed["review"]["verdict"], "rejected");
    assert_eq!(closed["review"]["targets_this_ready"], true);
    assert_eq!(closed["review"]["artifact_rechecked_at_close"], false);
    assert_eq!(
        closed["review"]["caveats"],
        limits.as_array().unwrap().len(),
        "the close reports how many limits the review recorded"
    );

    input(
        &mut h,
        lead,
        "continue after the close",
        vec![text_response("prefix reusable after a close with limits")],
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
    input(
        &mut h,
        lead,
        "close again after the restore",
        vec![
            call("close-replayed", "lane_close", close_args),
            text_response("the close survived the restore"),
        ],
        &mut events,
    )
    .await;
    let replayed = result(&h, "close-replayed");
    assert_eq!(replayed["outcome"], "already_closed", "{replayed}");
    assert_eq!(replayed["close_id"], closed["close_id"]);
    assert_eq!(replayed["review"], closed["review"]);
    input(
        &mut h,
        lead,
        "continue after the restore",
        vec![text_response("prefix reusable after the restore")],
        &mut events,
    )
    .await;
    for run in [lead, lane] {
        h.runtime.cancel(run).unwrap();
        h.runtime.wait(run).await.unwrap();
    }
    assert_eq!(h.mock.remaining_scripts(), 0);

    let requests = lead_only_requests(&h);
    assert_prefix_preserved(&requests);
    assert_result_unchanged(&requests, "close-limited", &closed);
    // Input-derived cache accounting of the lead's own requests, across the restore.
    let usage: Vec<_> = events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::Provider(ProviderEvent::RequestCompleted {
                run_id: Some(id),
                input_tokens,
                cache_read_tokens,
                ..
            }) if id == &lead.to_string() => Some((*input_tokens, *cache_read_tokens)),
            EventKind::Diagnostic(diagnostic) if diagnostic.code == "CacheRegression" => {
                panic!("{diagnostic:?}")
            }
            _ => None,
        })
        .collect();
    assert_eq!(usage.len(), requests.len());
    assert_eq!(usage[0].1, 0);
    for pair in usage.windows(2) {
        assert!(pair[0].0 > 0);
        assert!(
            pair[1].1 >= pair[0].0,
            "a request reuses everything the previous one sent: {pair:?}"
        );
    }
}
