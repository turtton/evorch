use super::*;
use crate::lane_delivery::{LaneDeliveryHost, LaneDeliveryTarget};
use storage::lane_delivery::LaneDeliveryNack;
struct Host {
    target: LaneDeliveryTarget,
    thread: String,
}
impl LaneDeliveryHost for Host {
    fn resolve(&self, thread: &str) -> Result<LaneDeliveryTarget, LaneDeliveryNack> {
        if thread != self.thread {
            return Err(LaneDeliveryNack::UnknownThread);
        }
        Ok(self.target.clone())
    }
    fn now_ns(&self) -> i64 {
        100
    }
}
async fn child(f: &Fixture, root: RunId) -> RunId {
    let run = f
        .runtime
        .spawn_reserved_child(
            root,
            f.runtime.reserve_run_id(),
            Role::Worker,
            "child",
            RunConfig::default(),
        )
        .unwrap();
    f.runtime.wait(run).await.unwrap();
    run
}
fn question(f: &Fixture, child: RunId) -> UserQuestion {
    f.runtime
        .request_user_question(child, "scope?".into(), vec![], false)
        .unwrap()
}
fn forward(f: &Fixture, root: RunId, q: &UserQuestion) -> UserQuestion {
    f.runtime
        .forward_lane_question(root, &q.id, "forward")
        .unwrap();
    f.runtime.user_question(&q.id).unwrap().unwrap()
}
#[tokio::test]
async fn forwarded_answer_reaches_original_and_inherited_children_without_new_capacity() {
    let mut f = Fixture::new();
    let lead = f.lead("lead").await;
    let lane = f.lane(lead, "lane", LanePolicy::Attached).await;
    let original = child(&f, lane.root_run_id).await;
    let inherited = child(&f, lane.root_run_id).await;
    let before = question(&f, original);
    f.db.handle()
        .bind_user_questions(
            &original.to_string(),
            &inherited.to_string(),
            std::slice::from_ref(&before.id),
        )
        .unwrap();
    for _ in 1..32 {
        question(&f, original);
    }
    let routed = forward(&f, lane.root_run_id, &before);
    assert_eq!(routed.run_id, before.run_id);
    assert_eq!(routed.root_run_id, before.root_run_id);
    assert_eq!(routed.root_name, before.root_name);
    assert_eq!(routed.recipient_run_ids, [inherited.to_string()]);
    assert_eq!(f.runtime.user_answers(original).unwrap().len(), 32);
    assert!(f.runtime.user_answers(lead).unwrap().is_empty());
    assert!(
        f.runtime
            .request_user_question(original, "33rd".into(), vec![], false)
            .is_err()
    );
    let retry = f
        .runtime
        .forward_lane_question(lane.root_run_id, &before.id, "forward")
        .unwrap();
    assert_eq!(retry.routing_revision, 1);
    assert_eq!(
        f.runtime
            .forward_lane_question(lane.root_run_id, &before.id, "second")
            .unwrap_err()
            .code,
        "stale_routing"
    );
    for err in [
        f.runtime
            .answer_user_question(&before.id, "host")
            .unwrap_err(),
        f.runtime
            .answer_subagent_question(lane.root_run_id, &before.id, "parent")
            .await
            .unwrap_err(),
    ] {
        assert_eq!(err, "not_responder");
    }
    assert_eq!(
        f.runtime.user_question(&before.id).unwrap().unwrap(),
        routed
    );
    let answered = f
        .runtime
        .resolve_lane_question(lead, request(&routed, "answer", Some("lead context")))
        .unwrap();
    assert!(matches!(
        answered.answer_origin,
        Some(AnswerOrigin::LaneLead { .. })
    ));
    let host = Host {
        target: LaneDeliveryTarget {
            root_run_id: lane.root_run_id,
            generation: 1,
            authority: RunConfig {
                conversation: true,
                ..Default::default()
            },
        },
        thread: lane.thread_id.clone(),
    };
    f.runtime
        .bind_lane_delivery_host(Arc::new(host))
        .unwrap()
        .abort();
    f.runtime
        .drive_lane_deliveries(&lane.thread_id)
        .await
        .unwrap();
    for run in [original, inherited] {
        assert_eq!(f.runtime.wait(run).await.unwrap(), AgentRunPhase::Done);
        assert_eq!(
            f.runtime.user_answers(run).unwrap()[0].answer,
            Some("lead context".into())
        );
        let saved = f
            .runtime
            .shared
            .run_store
            .get()
            .unwrap()
            .restore_record(run)
            .unwrap()
            .unwrap();
        let messages: Vec<providers::Message> = serde_json::from_str(&saved.messages_json).unwrap();
        assert!(AgentRuntime::answer_is_in_history(&messages, &answered));
    }
    let snapshots = || {
        [original, inherited].map(|run| {
            f.runtime
                .shared
                .run_store
                .get()
                .unwrap()
                .restore_record(run)
                .unwrap()
                .unwrap()
                .messages_json
        })
    };
    let after = snapshots();
    f.runtime
        .drive_lane_deliveries(&lane.thread_id)
        .await
        .unwrap();
    assert_eq!(
        snapshots(),
        after,
        "receipt replay cannot resume a child twice"
    );
    f.stop(lane.root_run_id).await;
    f.stop(lead).await;
}
#[tokio::test]
async fn host_reroute_race_is_storage_ordered_and_preserves_provenance() {
    let mut f = Fixture::new();
    let lead = f.lead("lead").await;
    let lane = f.lane(lead, "lane", LanePolicy::Attached).await;
    let run = child(&f, lane.root_run_id).await;
    let before = question(&f, run);
    let q = forward(&f, lane.root_run_id, &before);
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let a = barrier.clone();
    let rt = f.runtime.clone();
    let id = q.id.clone();
    let host = std::thread::spawn(move || {
        a.wait();
        rt.answer_user_question(&id, "human")
    });
    barrier.wait();
    f.runtime
        .resolve_lane_question(lead, request(&q, "route", None))
        .unwrap();
    if let Err(err) = host.join().unwrap() {
        assert_eq!(err, "not_responder");
        f.runtime.answer_user_question(&q.id, "human").unwrap();
    }
    let answer = f.runtime.user_answers(run).unwrap().remove(0);
    assert_eq!(answer.id, before.id);
    assert_eq!(answer.run_id, before.run_id);
    assert_eq!(answer.root_run_id, before.root_run_id);
    assert_eq!(answer.responder, Some(QuestionResponder::User));
    assert!(matches!(
        answer.answer_origin,
        Some(AnswerOrigin::Host { .. })
    ));
    assert_eq!(answer.answer.as_deref(), Some("human"));
    assert_eq!(answer.routing_revision, 3);
    f.runtime
        .forward_lane_question(lane.root_run_id, &q.id, "forward")
        .unwrap();
    assert_eq!(
        f.runtime.user_question(&q.id).unwrap().unwrap(),
        answer,
        "historical forward cannot restore Lead routing"
    );
    assert_eq!(
        f.db.handle()
            .lane_inbox(lane.thread_id, String::new())
            .unwrap()
            .len(),
        1
    );
    f.stop(lane.root_run_id).await;
    f.stop(lead).await;
}
#[tokio::test]
async fn forwarding_rejects_non_lane_summary_foreign_requester_and_answered_question() {
    let mut f = Fixture::new();
    let lead = f.lead("lead").await;
    let lane = f.lane(lead, "attached", LanePolicy::Attached).await;
    let summary = f.lane(lead, "summary", LanePolicy::Summary).await;
    let direct = child(&f, lead).await;
    let foreign = question(&f, direct);
    assert_eq!(
        f.runtime
            .forward_lane_question(lead, &foreign.id, "x")
            .unwrap_err()
            .code,
        "not_attached"
    );
    assert_eq!(
        f.runtime
            .forward_lane_question(lane.root_run_id, &foreign.id, "x")
            .unwrap_err()
            .code,
        "ownership_lost"
    );
    let run = child(&f, summary.root_run_id).await;
    let q = question(&f, run);
    assert_eq!(
        f.runtime
            .forward_lane_question(summary.root_run_id, &q.id, "x")
            .unwrap_err()
            .code,
        "not_attached"
    );
    let run = child(&f, lane.root_run_id).await;
    let q = question(&f, run);
    f.runtime.answer_user_question(&q.id, "early").unwrap();
    assert_eq!(
        f.runtime
            .forward_lane_question(lane.root_run_id, &q.id, "x")
            .unwrap_err()
            .code,
        "already_answered"
    );
    f.stop(summary.root_run_id).await;
    f.stop(lane.root_run_id).await;
    f.stop(lead).await;
}

#[tokio::test]
async fn stop_and_unavailable_delivery_do_not_transfer_answer_authority() {
    let mut f = Fixture::new();
    let lead = f.lead("lead").await;
    let lane = f.lane(lead, "stopped", LanePolicy::Attached).await;
    let run = child(&f, lane.root_run_id).await;
    let q = question(&f, run);
    let routed = forward(&f, lane.root_run_id, &q);
    f.runtime
        .stop(lane.root_run_id, crate::StopScope::SelfOnly)
        .unwrap();
    f.runtime.wait(lane.root_run_id).await.unwrap();
    assert_eq!(
        f.runtime
            .answer_user_question(&q.id, "bypass stop")
            .unwrap_err(),
        "not_responder"
    );
    assert_eq!(f.runtime.user_question(&q.id).unwrap().unwrap(), routed);
    f.runtime
        .resolve_lane_question(
            lead,
            request(&routed, "lead-after-stop", Some("saved while stopped")),
        )
        .unwrap();
    let host = Host {
        target: LaneDeliveryTarget {
            root_run_id: lane.root_run_id,
            generation: 1,
            authority: RunConfig {
                conversation: true,
                ..Default::default()
            },
        },
        thread: lane.thread_id.clone(),
    };
    f.runtime
        .bind_lane_delivery_host(Arc::new(host))
        .unwrap()
        .abort();
    f.runtime
        .drive_lane_deliveries(&lane.thread_id)
        .await
        .unwrap();
    assert_eq!(
        f.runtime.inspect_agent(lane.root_run_id).unwrap().phase,
        AgentRunPhase::Stopped
    );
    let saved = f
        .runtime
        .shared
        .run_store
        .get()
        .unwrap()
        .restore_record(run)
        .unwrap()
        .unwrap();
    let messages: Vec<providers::Message> = serde_json::from_str(&saved.messages_json).unwrap();
    let answered = f.runtime.user_question(&q.id).unwrap().unwrap();
    assert!(!AgentRuntime::answer_is_in_history(&messages, &answered));
    assert_eq!(answered.responder, routed.responder);
    assert_eq!(
        f.runtime
            .answer_user_question_if_pending(&q.id, "saved while stopped")
            .unwrap_err(),
        "not_responder"
    );
    f.stop(lead).await;
}
#[tokio::test]
async fn old_lane_restores_with_new_schema_and_subsequent_turns_keep_it_stable() {
    let mut f = Fixture::new();
    let lead = f.lead("lead").await;
    let lane = f.lane(lead, "version", LanePolicy::Attached).await;
    f.runtime
        .stop(lane.root_run_id, crate::StopScope::SelfOnly)
        .unwrap();
    f.runtime.wait(lane.root_run_id).await.unwrap();
    let mut record = f
        .runtime
        .shared
        .run_store
        .get()
        .unwrap()
        .restore_record(lane.root_run_id)
        .unwrap()
        .unwrap();
    let mut descriptor: crate::restore::RunRestoreDescriptor =
        serde_json::from_str(&record.config_json).unwrap();
    descriptor
        .tool_names
        .retain(|name| name != "lane_forward_question");
    record.config_json = serde_json::to_string(&descriptor).unwrap();
    f.db.handle().upsert_run_context(&record).unwrap();
    let start = f.model.requests.lock().unwrap().len();
    let authority = f.runtime.entry(lane.root_run_id).unwrap().config.clone();
    f.runtime.resume_chat(lane.root_run_id, authority).unwrap();
    f.waiting(lane.root_run_id).await;
    for _ in 0..2 {
        f.runtime
            .send_message(lane.root_run_id, "ordinary followup".into())
            .unwrap();
        f.waiting(lane.root_run_id).await;
    }
    {
        let tools = f.model.toolsets.lock().unwrap();
        let trace = &tools[start..];
        // History-only resume is idle; these are the two explicit followups.
        assert_eq!(trace.len(), 2);
        assert!(trace[0].iter().any(|t| t.name == "lane_forward_question"));
        for pair in trace.windows(2) {
            assert_eq!(pair[0], pair[1]);
        }
    }
    {
        let requests = f.model.requests.lock().unwrap();
        for pair in requests[start..].windows(2) {
            assert!(pair[1].starts_with(&pair[0]));
        }
    }
    f.stop(lane.root_run_id).await;
    f.stop(lead).await;
}
