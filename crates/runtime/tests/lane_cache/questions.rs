//! B/C's headless routing/tool cache contract.
//! Runtime host binding selects the responder; actual ask_user/tool delivery
//! must extend the lead provider prefix without rewriting earlier tool results.
use super::*;
use event_bus::lane::QuestionResponder;
use event_bus::{AnswerOrigin, UserQuestion};

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
                .any(|t| t["function"]["name"] == "lane_open")
        })
        .collect()
}
struct Host {
    target: runtime::lane_delivery::LaneDeliveryTarget,
    held: std::sync::atomic::AtomicBool,
}
impl runtime::lane_delivery::LaneDeliveryHost for Host {
    fn resolve(
        &self,
        thread: &str,
    ) -> Result<runtime::lane_delivery::LaneDeliveryTarget, storage::lane_delivery::LaneDeliveryNack>
    {
        if self.held.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(storage::lane_delivery::LaneDeliveryNack::Readonly);
        }
        if thread != "lead" {
            return Err(storage::lane_delivery::LaneDeliveryNack::UnknownThread);
        }
        Ok(self.target.clone())
    }
    fn now_ns(&self) -> i64 {
        100
    }
}
async fn bound_question(
    h: &mut Harness,
    lane: RunId,
    id: &str,
    events: &mut Vec<Event>,
) -> UserQuestion {
    input(
        h,
        lane,
        "ask the managing lead",
        vec![
            call(id, "ask_user", json!({"title":id,"blocking":false})),
            text_response("asked bound"),
        ],
        events,
    )
    .await;
    let response = result(h, id);
    let question = h
        .runtime
        .user_question(response["question_id"].as_str().unwrap())
        .unwrap()
        .unwrap();
    assert!(matches!(
        question.responder,
        Some(QuestionResponder::Lead { .. })
    ));
    question
}
fn answer_input(q: &UserQuestion, operation: &str, action: Value) -> Value {
    json!({"question_id":q.id,"management_revision":q.management_revision.unwrap(),
        "routing_revision":q.routing_revision,"operation_id":operation,"action":action})
}
#[tokio::test]
async fn attached_questions_list_answer_and_user_reroute_preserve_lead_cache_prefix() {
    let mut h = harness(vec![text_response("lead idle")], 1_000_000);
    let config = storage::StorageConfig {
        db_path: h._directory.path().join("lane-questions.db"),
        ..Default::default()
    };
    let db = storage::Storage::open(config.clone()).unwrap();
    h.runtime = h
        .runtime
        .with_run_store(runtime::RunStore::open(&config, db.handle()).unwrap());
    let lead = h
        .runtime
        .delegate_chat(
            "lead",
            Role::Orchestrator,
            "coordinate".into(),
            RunConfig {
                interactive: true,
                keep_alive: true,
                conversation: true,
                ..Default::default()
            },
        )
        .unwrap();
    let mut events = through_phase(&mut h.receiver, lead, AgentRunPhase::Waiting).await;
    let lane = open(&mut h, lead, "open", "routing cache lane", &mut events).await;
    input(
        &mut h,
        lane,
        "ask an unbound question",
        vec![
            call(
                "unbound",
                "ask_user",
                json!({"title":"unbound?","blocking":false}),
            ),
            text_response("asked"),
        ],
        &mut events,
    )
    .await;
    let unbound = result(&h, "unbound");
    assert_eq!(unbound["status"], "pending");
    assert!(unbound["delivery"].as_str().unwrap().contains("user"));
    let template = h
        .runtime
        .user_question(unbound["question_id"].as_str().unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(template.responder, Some(QuestionResponder::User));
    let host = Arc::new(Host {
        target: runtime::lane_delivery::LaneDeliveryTarget {
            root_run_id: lead,
            generation: 1,
            authority: RunConfig {
                conversation: true,
                ..Default::default()
            },
        },
        held: std::sync::atomic::AtomicBool::new(true),
    });
    h.runtime
        .bind_lane_delivery_host(host.clone())
        .unwrap()
        .abort();
    let answered = bound_question(&mut h, lane, "question-cache-agent", &mut events).await;
    let routed = bound_question(&mut h, lane, "question-cache-human", &mut events).await;
    // The lead receives both durable questions as one bounded append. Saved
    // answers remain held on the explicitly stopped lane until user resumption.
    h.runtime.stop(lane, runtime::StopScope::SelfOnly).unwrap();
    events.extend(through_phase(&mut h.receiver, lane, AgentRunPhase::Stopped).await);
    h.runtime.wait(lane).await.unwrap();
    h.mock
        .enqueue(vec![text_response("received bound notifications")]);
    host.held.store(false, std::sync::atomic::Ordering::SeqCst);
    h.runtime.drive_pending_lane_deliveries().await.unwrap();
    events.extend(through_phase(&mut h.receiver, lead, AgentRunPhase::Waiting).await);
    input(
        &mut h,
        lead,
        "read questions",
        vec![
            call("questions", "lane_questions", json!({})),
            text_response("listed"),
        ],
        &mut events,
    )
    .await;
    let listed = result(&h, "questions");
    assert_eq!(listed["questions"].as_array().unwrap().len(), 2);
    input(
        &mut h,
        lead,
        "answer from task context",
        vec![
            call(
                "answer",
                "lane_answer",
                answer_input(
                    &answered,
                    "agent-op",
                    json!({"kind":"answer","text":"agent scope"}),
                ),
            ),
            text_response("answered"),
        ],
        &mut events,
    )
    .await;
    assert_eq!(result(&h, "answer")["status"], "answer_saved");
    let stored = h.runtime.user_question(&answered.id).unwrap().unwrap();
    assert!(matches!(
        stored.answer_origin,
        Some(AnswerOrigin::LaneLead { .. })
    ));
    input(
        &mut h,
        lead,
        "let the user decide",
        vec![
            call(
                "route",
                "lane_answer",
                answer_input(&routed, "route-op", json!({"kind":"route_to_user"})),
            ),
            text_response("routed"),
        ],
        &mut events,
    )
    .await;
    assert_eq!(result(&h, "route")["question"]["id"], routed.id);
    let human = h
        .runtime
        .answer_user_question(&routed.id, "actual user decision")
        .unwrap();
    assert!(matches!(
        human.answer_origin,
        Some(AnswerOrigin::Host { .. })
    ));
    input(
        &mut h,
        lead,
        "read persistent notifications",
        vec![
            call("notices", "lane_list", json!({"notifications_after":""})),
            text_response("read references"),
        ],
        &mut events,
    )
    .await;
    assert!(result(&h, "notices")["notifications"].is_array());
    input(
        &mut h,
        lead,
        "ordinary followup",
        vec![text_response("still stable")],
        &mut events,
    )
    .await;
    let requests = lead_requests(&h);
    assert!(requests.len() >= 9);
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
        assert!(pair[0].0 > 0);
        assert!(pair[1].1 >= pair[0].0);
    }
    assert_eq!(
        result(&h, "questions"),
        listed,
        "already-sent question lists must not be rewritten after mutation"
    );
    assert!(h.runtime.user_answers(lead).unwrap().is_empty());
    h.runtime.cancel(lead).unwrap();
    h.runtime.wait(lead).await.unwrap();
    assert_eq!(h.mock.remaining_scripts(), 0);

    // Preview-version boundary: replay captured real runtime wire inputs with
    // the two new tools removed to represent the 1b schema. A schema change is
    // intentionally NOT append-only; subsequent real 1c requests must be stable.
    let mut old = requests[0].clone();
    old["tools"].as_array_mut().unwrap().retain(|t| {
        !matches!(
            t["function"]["name"].as_str(),
            Some("lane_questions" | "lane_answer")
        )
    });
    assert!(assert_append_only(CacheProtocol::OpenAi, &old, &requests[0]).is_err());
    let server = StreamingMockOpenAi::spawn_with_prompt_cache_models(
        vec![
            text_response("old"),
            text_response("new"),
            text_response("next"),
        ],
        vec![MODEL.into()],
    );
    let client = reqwest::Client::new();
    let mut accounting = Vec::new();
    for body in [&old, &requests[0], &requests[1]] {
        let text = client
            .post(format!("{}/chat/completions", server.base_url()))
            .json(body)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .text()
            .await
            .unwrap();
        let usage = text
            .lines()
            .filter_map(|l| l.strip_prefix("data: "))
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .find_map(|v| v.get("usage").filter(|v| v.is_object()).cloned())
            .unwrap();
        accounting.push((
            usage["prompt_tokens"].as_u64().unwrap(),
            usage["prompt_tokens_details"]["cached_tokens"]
                .as_u64()
                .unwrap(),
        ));
    }
    assert!(
        accounting[1].1 < accounting[0].0,
        "version boundary must invalidate the old full input"
    );
    assert!(
        accounting[2].1 >= accounting[1].0,
        "reuse starts from the new version prefix"
    );
    let captured: Vec<_> = server
        .recorded_requests()
        .into_iter()
        .map(|r| r.body)
        .collect();
    assert_append_only(CacheProtocol::OpenAi, &captured[1], &captured[2]).unwrap();
}

#[tokio::test]
async fn ask_user_capacity_returns_a_bounded_success_without_retry() {
    let mut h = harness(vec![text_response("idle")], 1_000_000);
    let config = storage::StorageConfig {
        db_path: h._directory.path().join("capacity.db"),
        ..Default::default()
    };
    let db = storage::Storage::open(config.clone()).unwrap();
    h.runtime = h
        .runtime
        .with_run_store(runtime::RunStore::open(&config, db.handle()).unwrap());
    let lead = h
        .runtime
        .delegate_chat(
            "capacity-lead",
            Role::Orchestrator,
            "coordinate".into(),
            RunConfig {
                interactive: true,
                keep_alive: true,
                conversation: true,
                ..Default::default()
            },
        )
        .unwrap();
    let mut events = through_phase(&mut h.receiver, lead, AgentRunPhase::Waiting).await;
    for i in 0..32 {
        h.runtime
            .request_user_question(lead, format!("question {i}?"), vec![], false)
            .unwrap();
    }
    input(
        &mut h,
        lead,
        "ask one more",
        vec![
            call("capacity", "ask_user", json!({"title":"excess?"})),
            text_response("capacity understood, no retry"),
        ],
        &mut events,
    )
    .await;
    let blocked = result(&h, "capacity");
    assert_eq!(blocked["status"], "question_capacity_blocked");
    assert_eq!(blocked["limit"], 32);
    assert_eq!(blocked["retry_automatically"], false);
    assert!(blocked.get("question_id").is_none());
    assert!(blocked.to_string().len() < 512);
    assert_eq!(h.runtime.user_answers(lead).unwrap().len(), 32);
    let requests = lead_requests(&h);
    assert_eq!(requests.len(), 3);
    for pair in requests.windows(2) {
        assert_append_only(CacheProtocol::OpenAi, &pair[0], &pair[1]).unwrap();
    }
    let cached: Vec<_> = events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::Provider(ProviderEvent::RequestCompleted {
                input_tokens,
                cache_read_tokens,
                ..
            }) => Some((*input_tokens, *cache_read_tokens)),
            _ => None,
        })
        .collect();
    assert_eq!(cached.len(), 3);
    for pair in cached.windows(2) {
        assert!(pair[1].1 >= pair[0].0);
    }
    assert_eq!(h.mock.remaining_scripts(), 0);
    h.runtime.cancel(lead).unwrap();
    h.runtime.wait(lead).await.unwrap();
}

#[tokio::test]
async fn failed_report_retries_from_same_run_share_one_notification_episode() {
    let mut h = harness(vec![text_response("lead idle")], 1_000_000);
    let config = storage::StorageConfig {
        db_path: h._directory.path().join("failed-report.db"),
        ..Default::default()
    };
    let db = storage::Storage::open(config.clone()).unwrap();
    h.runtime = h
        .runtime
        .with_run_store(runtime::RunStore::open(&config, db.handle()).unwrap());
    let lead = h
        .runtime
        .delegate_chat(
            "failed-report-lead",
            Role::Orchestrator,
            "coordinate".into(),
            RunConfig {
                interactive: true,
                keep_alive: true,
                conversation: true,
                ..Default::default()
            },
        )
        .unwrap();
    let mut events = through_phase(&mut h.receiver, lead, AgentRunPhase::Waiting).await;
    let lane = open(&mut h, lead, "open-failed", "failure episode", &mut events).await;
    let mut reports = Vec::new();
    for call_id in ["failed-first", "failed-again"] {
        input(
            &mut h,
            lane,
            "report failed attempt",
            vec![
                call(
                    call_id,
                    "lane_report",
                    json!({"status":"failed","summary":call_id}),
                ),
                text_response("reported"),
            ],
            &mut events,
        )
        .await;
        let report = result(&h, call_id);
        assert_eq!(report["status"], "failed");
        assert_eq!(report["failure_episode"], format!("agent:{lane}"));
        reports.push(report);
    }
    assert_eq!(reports[0]["failure_episode"], reports[1]["failure_episode"]);
    assert_ne!(
        reports[0]["report_seq"], reports[1]["report_seq"],
        "both report writes are retained"
    );
    input(
        &mut h,
        lead,
        "read notification references",
        vec![
            call(
                "failure-notices",
                "lane_list",
                json!({"notifications_after":""}),
            ),
            text_response("read"),
        ],
        &mut events,
    )
    .await;
    let notices = result(&h, "failure-notices");
    let failed: Vec<_> = notices["notifications"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|n| n["kind"]["kind"] == "failed")
        .collect();
    assert_eq!(
        failed.len(),
        1,
        "a repeated failure episode queues exactly one notification"
    );
    for run in [lane, lead] {
        h.runtime.cancel(run).unwrap();
        h.runtime.wait(run).await.unwrap();
    }
    assert_eq!(h.mock.remaining_scripts(), 0);
}
