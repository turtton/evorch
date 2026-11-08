//! Real provider wire + SQLite. Management ingress is called directly (3E owns tools).
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
    let ctx = LaneManagementContext::Run(lead);
    for (revision, policy) in [
        (1, ManagedLanePolicy::Summary),
        (2, ManagedLanePolicy::Attached),
    ] {
        let changed = h
            .runtime
            .set_lane_policy(
                ctx,
                request(
                    &record.lane_id,
                    revision,
                    LaneManagementAction::SetPolicy { policy },
                ),
            )
            .unwrap();
        assert_eq!(
            changed.current_lane.unwrap().root_run_id,
            Some(lane.to_string())
        );
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
    h.runtime
        .detach_lane(
            ctx,
            request(&record.lane_id, 3, LaneManagementAction::Detach),
        )
        .unwrap();
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
