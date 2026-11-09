//! Public management tools on real provider wire + SQLite; reattach stays human-only.
use super::*;
use event_bus::lane::QuestionResponder;
use runtime::lane_management::*;

struct HeldHost;
impl runtime::lane_delivery::LaneDeliveryHost for HeldHost {
    fn resolve(
        &self,
        _: &str,
    ) -> Result<runtime::lane_delivery::LaneDeliveryTarget, storage::lane_delivery::LaneDeliveryNack>
    {
        Err(storage::lane_delivery::LaneDeliveryNack::Readonly)
    }
}
fn request(lane_id: &str, revision: u64, action: LaneManagementAction) -> LaneManagementRequest {
    LaneManagementRequest {
        operation_id: format!("cache-management-{revision}"),
        lane_id: lane_id.into(),
        expected_management_revision: revision,
        action,
    }
}
#[tokio::test]
async fn same_run_policy_roundtrip_detach_and_reattach_preserve_brief_question_and_tool_prefixes() {
    let mut h = harness(vec![text_response("lead idle")], 1_000_000);
    let config = storage::StorageConfig {
        db_path: h._directory.path().join("management.db"),
        ..Default::default()
    };
    let db = storage::Storage::open(config.clone()).unwrap();
    h.runtime = h
        .runtime
        .with_run_store(runtime::RunStore::open(&config, db.handle()).unwrap());
    let lead = h
        .runtime
        .delegate_chat(
            "management-lead",
            Role::Orchestrator,
            "coordinate management cache".into(),
            RunConfig {
                interactive: true,
                keep_alive: true,
                conversation: true,
                ..Default::default()
            },
        )
        .unwrap();
    let mut events = through_phase(&mut h.receiver, lead, AgentRunPhase::Waiting).await;
    let lane = open(
        &mut h,
        lead,
        "cache-open",
        "immutable management brief",
        &mut events,
    )
    .await;
    let record = db
        .handle()
        .lane_by_thread(&format!("lane-thread-{lane}"))
        .unwrap()
        .unwrap();
    h.runtime
        .bind_lane_delivery_host(Arc::new(HeldHost))
        .unwrap()
        .abort();
    input(
        &mut h,
        lane,
        "ask before management changes",
        vec![
            call(
                "old-question",
                "ask_user",
                json!({"title":"immutable management question","blocking":false}),
            ),
            text_response("asked"),
        ],
        &mut events,
    )
    .await;
    let qid = result(&h, "old-question")["question_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let original = h.runtime.user_question(&qid).unwrap().unwrap();
    assert!(matches!(
        original.responder,
        Some(QuestionResponder::Lead { .. })
    ));
    for (revision, policy) in [(1, "summary"), (2, "attached")] {
        let call_id = format!("policy-{revision}");
        input(
            &mut h,
            lead,
            "change policy using the public tool",
            vec![
                call(
                    &call_id,
                    "lane_set_policy",
                    json!({"operation_id":format!("policy-{revision}"),
                "lane_id":record.lane_id,"expected_management_revision":revision,"policy":policy}),
                ),
                text_response("policy changed"),
            ],
            &mut events,
        )
        .await;
        let changed = result(&h, &call_id);
        assert_eq!(changed["current_lane"]["root_run_id"], lane.to_string());
        input(
            &mut h,
            lane,
            "continue same run",
            vec![
                call(&format!("answers-{revision}"), "user_answers", json!({})),
                text_response("continued"),
            ],
            &mut events,
        )
        .await;
    }
    let routed = h.runtime.user_question(&qid).unwrap().unwrap();
    assert_eq!(routed.responder, Some(QuestionResponder::User));
    assert_eq!((routed.id, routed.run_id), (original.id, original.run_id));
    input(&mut h, lead, "detach using the public tool", vec![
        call("tool-detach", "lane_detach", json!({"operation_id":"tool-detach", "lane_id":record.lane_id, "expected_management_revision":3})),
        text_response("detached"),
    ], &mut events).await;
    assert_eq!(
        result(&h, "tool-detach")["current_lane"]["policy"],
        "detached"
    );
    input(
        &mut h,
        lane,
        "detached continuation",
        vec![
            call("detached-answers", "user_answers", json!({})),
            text_response("independent"),
        ],
        &mut events,
    )
    .await;
    // Model-facing tools remain fixed, but management-only Report execution fails.
    input(
        &mut h,
        lane,
        "try detached report",
        vec![
            call(
                "detached-report",
                "lane_report",
                json!({"status":"ready","summary":"must not save"}),
            ),
            text_response("rejected"),
        ],
        &mut events,
    )
    .await;
    assert_eq!(
        db.handle()
            .lane(&record.lane_id)
            .unwrap()
            .unwrap()
            .latest_report_seq,
        0
    );
    // Host has authoritative project/family state. No schema change or run restart.
    rusqlite::Connection::open(&config.db_path)
        .unwrap()
        .execute("UPDATE threads SET project_id='management-project'", [])
        .unwrap();
    let outcome = h
        .runtime
        .reattach_lane(
            LaneHumanContext,
            request(
                &record.lane_id,
                4,
                LaneManagementAction::Reattach {
                    lead_thread_id: "management-lead".into(),
                    expected_lead_root_run_id: lead.to_string(),
                },
            ),
            LaneAttachGuard {
                pinned: false,
                archived: false,
            },
        )
        .unwrap();
    assert_eq!(
        outcome.operation.lane.policy,
        event_bus::LanePolicy::Summary
    );
    input(
        &mut h,
        lane,
        "reattached continuation",
        vec![
            call("reattached-answers", "user_answers", json!({})),
            text_response("reattached"),
        ],
        &mut events,
    )
    .await;
    input(
        &mut h,
        lead,
        "inspect management ledger",
        vec![
            call("management-list", "lane_list", json!({})),
            text_response("observed"),
        ],
        &mut events,
    )
    .await;
    for id in [lane, lead] {
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
    for (run, is_lane) in [(lead, false), (lane, true)] {
        let group: Vec<_> = requests
            .iter()
            .filter(|r| {
                r["tools"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|t| t["function"]["name"] == "lane_report")
                    == is_lane
            })
            .cloned()
            .collect();
        assert!(group.len() >= 2);
        for pair in group.windows(2) {
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
                }) if id == &run.to_string() => Some((*input_tokens, *cache_read_tokens)),
                EventKind::Diagnostic(d) if d.code == "CacheRegression" => panic!("{d:?}"),
                _ => None,
            })
            .collect();
        assert_eq!(usage.len(), group.len());
        for pair in usage.windows(2) {
            assert!(pair[0].0 > 0);
            assert!(
                pair[1].1 >= pair[0].0,
                "input-derived cache must reuse the sent prefix"
            );
        }
        if is_lane {
            let first = group[0]["messages"]
                .as_array()
                .unwrap()
                .iter()
                .find(|m| m["role"] == "user")
                .unwrap();
            assert!(first.to_string().contains("immutable management brief"));
            let before = group
                .iter()
                .find(|r| {
                    r["messages"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|m| m["tool_call_id"] == "old-question")
                })
                .unwrap();
            let last = group.last().unwrap();
            let old = before["messages"]
                .as_array()
                .unwrap()
                .iter()
                .find(|m| m["tool_call_id"] == "old-question")
                .unwrap();
            let kept = last["messages"]
                .as_array()
                .unwrap()
                .iter()
                .find(|m| m["tool_call_id"] == "old-question")
                .unwrap();
            assert_eq!(
                old, kept,
                "sent question/tool result is immutable despite rerouting"
            );
        }
    }
}

#[tokio::test]
async fn public_management_tools_reject_lane_worker_child_and_non_lead_actors() {
    let mut h = harness(vec![text_response("lead idle")], 1_000_000);
    let config = storage::StorageConfig {
        db_path: h._directory.path().join("actors.db"),
        ..Default::default()
    };
    let db = storage::Storage::open(config.clone()).unwrap();
    h.runtime = h
        .runtime
        .with_run_store(runtime::RunStore::open(&config, db.handle()).unwrap());
    let config_run = RunConfig {
        conversation: true,
        interactive: true,
        keep_alive: true,
        ..Default::default()
    };
    let lead = h
        .runtime
        .delegate_chat(
            "actor-lead",
            Role::Orchestrator,
            "ACTOR-LEAD".into(),
            config_run.clone(),
        )
        .unwrap();
    let mut events = through_phase(&mut h.receiver, lead, AgentRunPhase::Waiting).await;
    let lane = open(&mut h, lead, "actor-open", "actor lane", &mut events).await;
    let record = db
        .handle()
        .lane_by_thread(&format!("lane-thread-{lane}"))
        .unwrap()
        .unwrap();
    let args = |id: &str| json!({"operation_id":id,"lane_id":record.lane_id,"expected_management_revision":1,"policy":"summary"});
    input(
        &mut h,
        lane,
        "lane cannot manage itself",
        vec![
            call("lane-self", "lane_set_policy", args("lane-self")),
            text_response("lane denied"),
        ],
        &mut events,
    )
    .await;
    h.mock.enqueue([
        call("worker", "lane_set_policy", args("worker")),
        text_response("worker denied"),
    ]);
    let worker = h
        .runtime
        .delegate_chat(
            "actor-worker",
            Role::Worker,
            "ACTOR-WORKER".into(),
            config_run.clone(),
        )
        .unwrap();
    events.extend(through_phase(&mut h.receiver, worker, AgentRunPhase::Waiting).await);
    h.mock.enqueue([
        call("child", "lane_set_policy", args("child")),
        text_response("child denied"),
    ]);
    let child = h
        .runtime
        .delegate_background_as_child(lane, Role::Worker, "ACTOR-CHILD", config_run.clone())
        .unwrap();
    events.extend(through_phase(&mut h.receiver, child, AgentRunPhase::Waiting).await);
    h.mock.enqueue([
        call("other-root", "lane_set_policy", args("other-root")),
        text_response("non-lead denied"),
    ]);
    let other = h
        .runtime
        .delegate_chat(
            "actor-other",
            Role::Orchestrator,
            "ACTOR-OTHER".into(),
            config_run,
        )
        .unwrap();
    events.extend(through_phase(&mut h.receiver, other, AgentRunPhase::Waiting).await);
    for id in ["lane-self", "worker", "child", "other-root"] {
        assert!(events.iter().any(|event| matches!(&event.kind, EventKind::Tool(event_bus::ToolEvent::ToolCompleted { call_id, is_error:true, .. }) if call_id == id)), "{id} must fail at the public execution boundary");
    }
    assert_eq!(
        db.handle()
            .lane(&record.lane_id)
            .unwrap()
            .unwrap()
            .management_revision,
        1
    );
    for run in [child, worker, other, lane, lead] {
        h.runtime.cancel(run).unwrap();
        h.runtime.wait(run).await.unwrap();
    }
}
