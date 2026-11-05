//! B/C integration: real model tool calls, durable storage and both root wakes.
use super::*;
use event_bus::{AnswerOrigin, lane::QuestionResponder};
struct Hosts {
    targets: HashMap<String, LaneDeliveryTarget>,
    hold_lead: AtomicBool,
    restores: Mutex<HashMap<RunId, usize>>,
}
impl LaneDeliveryHost for Hosts {
    fn resolve(&self, thread: &str) -> Result<LaneDeliveryTarget, LaneDeliveryNack> {
        if thread == "delivery-lead" && self.hold_lead.load(Ordering::SeqCst) {
            return Err(LaneDeliveryNack::Readonly);
        }
        self.targets
            .get(thread)
            .cloned()
            .ok_or(LaneDeliveryNack::UnknownThread)
    }
    fn restore_root(
        &self,
        runtime: &AgentRuntime,
        target: &LaneDeliveryTarget,
    ) -> Result<(), LaneDeliveryNack> {
        *self
            .restores
            .lock()
            .unwrap()
            .entry(target.root_run_id)
            .or_default() += 1;
        runtime
            .wake_chat(target.root_run_id, target.authority.clone())
            .map(|_| ())
            .map_err(|_| LaneDeliveryNack::RestoreFailed)
    }
    fn now_ns(&self) -> i64 {
        100
    }
}
#[tokio::test]
async fn bound_question_wakes_lead_and_agent_answer_wakes_lane_without_user_permission() {
    let mut f = Fixture::new();
    let (lead, old) = f.lead().await;
    let lane = f.lane(lead, "question-roundtrip").await;
    f.finish(lead).await;
    let host = Arc::new(Hosts {
        targets: HashMap::from([
            ("delivery-lead".into(), old.target.clone()),
            (
                lane.thread_id.clone(),
                LaneDeliveryTarget {
                    root_run_id: lane.root_run_id,
                    ..old.target.clone()
                },
            ),
        ]),
        hold_lead: AtomicBool::new(true),
        restores: Mutex::new(HashMap::new()),
    });
    if let Some(worker) = f.runtime.bind_lane_delivery_host(host.clone()) {
        worker.abort();
    }
    assert_eq!(
        f.runtime.lane_delivery_binding(),
        LaneDeliveryBinding::Bound
    );
    f.model.next_tools.lock().unwrap().insert(
        lane.root_run_id.to_string(),
        ContentBlock::ToolUse {
            id: "ask-bound".into(),
            name: "ask_user".into(),
            input: serde_json::json!({"title":"Which task scope?","blocking":false}),
        },
    );
    f.runtime
        .send_message(lane.root_run_id, "ask the managing lead".into())
        .unwrap();
    f.runtime.wait(lane.root_run_id).await.unwrap();
    assert_eq!(
        f.runtime.inspect_agent(lane.root_run_id).unwrap().phase,
        AgentRunPhase::Done
    );
    let question = f
        .storage
        .handle()
        .lane_questions("delivery-lead".into(), None)
        .unwrap()
        .questions
        .into_iter()
        .next()
        .unwrap();
    assert!(matches!(
        question.responder,
        Some(QuestionResponder::Lead { .. })
    ));
    f.model.finish.store(false, Ordering::SeqCst);
    host.hold_lead.store(false, Ordering::SeqCst);
    f.runtime.drive_pending_lane_deliveries().await.unwrap();
    f.phase(lead, AgentRunPhase::Waiting).await;
    let visible_before =
        f.runtime.shared.review_runs.lock().unwrap()[&lane.root_run_id].visible_request_count();
    f.model.next_tools.lock().unwrap().insert(
        lead.to_string(),
        ContentBlock::ToolUse {
            id: "answer-bound".into(),
            name: "lane_answer".into(),
            input: serde_json::json!({
                "question_id":question.id,"management_revision":question.management_revision.unwrap(),
                "routing_revision":question.routing_revision,"operation_id":"roundtrip-answer",
                "action":{"kind":"answer","text":"agent task-scope decision"}
            }),
        },
    );
    f.model.next_tools.lock().unwrap().insert(
        lane.root_run_id.to_string(),
        ContentBlock::ToolUse {
            id: "read-bound-answer".into(),
            name: "user_answers".into(),
            input: serde_json::json!({}),
        },
    );
    f.runtime
        .send_message(lead, "answer the pending lane question".into())
        .unwrap();
    let mut pending = HashSet::from([lead.to_string(), lane.root_run_id.to_string()]);
    while !pending.is_empty() {
        if let EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
            run_id,
            to,
            reason,
            ..
        }) = f.events.recv().await.unwrap().kind
        {
            assert!(
                !pending.contains(&run_id) || to != AgentRunPhase::Error,
                "{reason:?}"
            );
            if to == AgentRunPhase::Waiting {
                pending.remove(&run_id);
            }
        }
    }
    let stored = f.runtime.user_question(&question.id).unwrap().unwrap();
    assert!(matches!(
        stored.answer_origin,
        Some(AnswerOrigin::LaneLead { .. })
    ));
    assert_eq!(
        f.runtime.shared.review_runs.lock().unwrap()[&lane.root_run_id].visible_request_count(),
        visible_before,
        "agent answer delivery must not create real-user permission evidence"
    );
    let requests = f.model.requests.lock().unwrap().clone();
    let delivered = |kind: &str, needle: &str| {
        requests
            .iter()
            .flatten()
            .filter(|m| m.role == providers::Role::User)
            .flat_map(|m| &m.content)
            .filter_map(|b| match b {
                ContentBlock::Text { text }
                    if text.starts_with("[Agent-authored lane notifications;") =>
                {
                    Some(text)
                }
                _ => None,
            })
            .flat_map(|text| text.lines())
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .any(|v| v["kind"]["kind"] == kind && v["kind"]["question_id"] == needle)
    };
    assert!(delivered("question", &question.id));
    assert!(delivered("answer", &question.id));
    assert!(
        requests.iter().flatten().any(|m| {
            let text = serde_json::to_string(m).unwrap();
            text.contains("read-bound-answer") && text.contains("agent task-scope decision")
        }),
        "restored lane must be able to read the stored agent answer"
    );
    drop(requests);
    assert_eq!(host.restores.lock().unwrap().get(&lead), Some(&1));
    assert_eq!(
        host.restores.lock().unwrap().get(&lane.root_run_id),
        Some(&1)
    );
    // Delivery metadata is separate from real user input and never answers as Host.
    assert!(f.receipts() >= 2);
    for root in [lead, lane.root_run_id] {
        f.runtime.cancel(root).unwrap();
        f.runtime.wait(root).await.unwrap();
    }
}

#[tokio::test]
async fn active_turn_delivery_is_not_a_completed_root_wake_or_finish_gate() {
    let mut f = Fixture::new();
    let (lead, host) = f.lead().await;
    let lane = f.lane(lead, "active-delivery").await;
    f.question(&lane, 0);
    assert_eq!(
        f.drive().await,
        LaneDeliveryOutcome::Delivered { notifications: 1 }
    );
    f.phase(lead, AgentRunPhase::Waiting).await;
    f.report(&lane, Some(1), LaneStatus::Ready);
    assert_eq!(
        f.drive().await,
        LaneDeliveryOutcome::Delivered { notifications: 1 }
    );
    f.phase(lead, AgentRunPhase::Waiting).await;
    assert_eq!(host.restores.load(Ordering::SeqCst), 0);
    assert_eq!(
        f.storage
            .handle()
            .lane_wake_guard(lane.lane_id)
            .unwrap()
            .no_progress_batches,
        0,
        "active delivery is governed by the running lead, not completed-root wake budget"
    );
    assert_eq!(
        f.storage
            .handle()
            .lane_questions("delivery-lead".into(), None)
            .unwrap()
            .questions
            .len(),
        1
    );
    // Neither an unanswered lane question nor unverified ready blocks this lead.
    f.finish(lead).await;
}
