//! PR 2E: child question → answer → nudge → Ready → verify → close through the HTTP cache.
use super::*;
use runtime::lane_delivery::{LaneDeliveryHost, LaneDeliveryTarget};
use std::sync::atomic::{AtomicBool, Ordering};
struct Host {
    lead: RunId,
    lane: RunId,
    thread: String,
    held: AtomicBool,
    lane_authority: RunConfig,
}
impl LaneDeliveryHost for Host {
    fn resolve(
        &self,
        thread: &str,
    ) -> Result<LaneDeliveryTarget, storage::lane_delivery::LaneDeliveryNack> {
        if self.held.load(Ordering::SeqCst) {
            return Err(storage::lane_delivery::LaneDeliveryNack::Readonly);
        }
        let root = if thread == "forward-cache-lead" {
            self.lead
        } else if thread == self.thread {
            self.lane
        } else {
            return Err(storage::lane_delivery::LaneDeliveryNack::UnknownThread);
        };
        Ok(LaneDeliveryTarget {
            root_run_id: root,
            generation: 1,
            authority: if root == self.lane {
                self.lane_authority.clone()
            } else {
                RunConfig {
                    conversation: true,
                    ..Default::default()
                }
            },
        })
    }
    fn now_ns(&self) -> i64 {
        100
    }
}
fn attention(config: &storage::StorageConfig) -> Vec<event_bus::lane::LaneAttentionItem> {
    storage::Database::open_read_only(&config.db_path)
        .unwrap()
        .lane_attention(
            event_bus::lane::LaneAttentionFilter {
                lead_thread_id: "forward-cache-lead".into(),
                project_id: None,
            },
            None,
            20,
        )
        .unwrap()
        .items
}
fn actor(request: &Value) -> usize {
    let names = request["tools"].as_array().unwrap();
    if names.iter().any(|t| t["function"]["name"] == "lane_open") {
        0
    } else if names
        .iter()
        .any(|t| t["function"]["name"] == "lane_forward_question")
    {
        1
    } else if names
        .iter()
        .any(|t| t["function"]["name"] == "submit_lane_verification")
    {
        3
    } else {
        2
    }
}
struct ReleaseOnDrop(mock_openai::ResponseGate);
impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        self.0.release();
    }
}
#[tokio::test]
async fn child_forward_answer_nudge_ready_verify_close_are_append_only() {
    let (read_tx, mut reads) = tokio::sync::mpsc::unbounded_channel();
    let mut h = harness_with_read(
        vec![text_response("lead idle")],
        1_000_000,
        Arc::new(super::verification::ReadGate(read_tx)),
    );
    let config = storage::StorageConfig {
        db_path: h._directory.path().join("forward-cache.db"),
        ..Default::default()
    };
    let db = storage::Storage::open(config.clone()).unwrap();
    h.runtime = h
        .runtime
        .with_run_store(runtime::RunStore::open(&config, db.handle()).unwrap());
    h.runtime
        .set_project_root(h._directory.path().to_owned())
        .unwrap();
    let lead = h
        .runtime
        .delegate_chat(
            "forward-cache-lead",
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
    let lane = open(
        &mut h,
        lead,
        "forward-open",
        "forward cache lane",
        &mut events,
    )
    .await;
    let thread = result(&h, "forward-open")["thread_id"]
        .as_str()
        .unwrap()
        .to_owned();
    h.mock.enqueue([
        call(
            "child-ask",
            "ask_user",
            json!({"title":"scope decision?","blocking":false}),
        ),
        text_response("independent child work complete"),
        text_response("lane observed child completion"),
    ]);
    let child = h
        .runtime
        .spawn_reserved_child(
            lane,
            h.runtime.reserve_run_id(),
            Role::Worker,
            "forward cache child",
            RunConfig::default(),
        )
        .unwrap();
    events.extend(through_phase(&mut h.receiver, child, AgentRunPhase::Done).await);
    h.runtime.wait(child).await.unwrap();
    events.extend(through_phase(&mut h.receiver, lane, AgentRunPhase::Waiting).await);
    let asked = result(&h, "child-ask");
    let id = asked["question_id"].as_str().unwrap().to_owned();
    let before = h.runtime.user_question(&id).unwrap().unwrap();
    input(
        &mut h,
        lane,
        "forward child question",
        vec![
            call(
                "forward",
                "lane_forward_question",
                json!({"question_id":id}),
            ),
            text_response("forwarded"),
        ],
        &mut events,
    )
    .await;
    let forwarded = result(&h, "forward");
    assert!(attention(&config).iter().any(|i| matches!(&i.kind, event_bus::lane::LaneAttentionKind::QuestionPending { question_id, .. } if question_id == &id)));
    assert_eq!(forwarded["question_id"], id);
    let q = h.runtime.user_question(&id).unwrap().unwrap();
    assert_eq!(q.run_id, before.run_id);
    assert_eq!(q.root_run_id, before.root_run_id);
    assert_eq!(q.recipient_run_ids, before.recipient_run_ids);
    assert_eq!(
        h.runtime
            .answer_user_question(&id, "host bypass")
            .unwrap_err(),
        "not_responder"
    );
    let owner = Arc::new(
        runtime::ownership::OwnerHost::open(
            h._directory.path(),
            Default::default(),
            Arc::new(event_bus::EventBus::new(128)),
        )
        .unwrap(),
    );
    let permit = owner.start(&thread).unwrap();
    let delivery_owner = storage::lane_delivery::LaneDeliveryOwner {
        root_run_id: lane.to_string(),
        generation: 1,
        ownership_generation: permit.lease.generation,
    };
    let host = Arc::new(Host {
        lead,
        lane,
        thread: thread.clone(),
        held: AtomicBool::new(false),
        lane_authority: RunConfig {
            ownership: Some(permit),
            conversation: true,
            ..Default::default()
        },
    });
    h.runtime
        .bind_lane_delivery_host(host.clone())
        .unwrap()
        .abort();
    h.mock
        .enqueue([text_response("lead received forwarded reference")]);
    h.runtime
        .drive_lane_deliveries("forward-cache-lead")
        .await
        .unwrap();
    events.extend(through_phase(&mut h.receiver, lead, AgentRunPhase::Waiting).await);
    host.held.store(true, Ordering::SeqCst);
    input(&mut h, lead, "answer child question", vec![call("forward-answer", "lane_answer", json!({"question_id":id,"management_revision":q.management_revision.unwrap(),"routing_revision":q.routing_revision,"operation_id":"answer-op","action":{"kind":"answer","text":"lead decision"}})), text_response("answered child")], &mut events).await;
    // Both recipients can request concurrently. Gate BOTH identical user_answers
    // responses so their following text responses cannot overtake the second call.
    let (gate, arrived) = mock_openai::ResponseGate::new();
    let _release = ReleaseOnDrop(gate.clone());
    // Both final requests must reach the wire before either run finishes.
    // Otherwise the child's completion can coalesce into the lane's final
    // request, nondeterministically removing the completion-only followup.
    let (final_gate, final_arrivals) = mock_openai::ResponseGate::new();
    let _final_release = ReleaseOnDrop(final_gate.clone());
    let start = events.len();
    h.mock.enqueue([
        call("return-answers", "user_answers", json!({})).with_gate(gate.clone()),
        call("return-answers", "user_answers", json!({})).with_gate(gate.clone()),
        text_response("answer observed").with_gate(final_gate.clone()),
        text_response("answer observed").with_gate(final_gate.clone()),
        text_response("lane observed resumed child completion"),
    ]);
    host.held.store(false, Ordering::SeqCst);
    h.runtime.drive_lane_deliveries(&thread).await.unwrap();
    tokio::task::spawn_blocking(move || {
        arrived.recv().unwrap();
        arrived.recv().unwrap();
    })
    .await
    .unwrap();
    gate.release();
    tokio::task::spawn_blocking(move || {
        final_arrivals.recv().unwrap();
        final_arrivals.recv().unwrap();
    })
    .await
    .unwrap();
    final_gate.release();
    events.extend(through_phase(&mut h.receiver, child, AgentRunPhase::Done).await);
    h.runtime.wait(child).await.unwrap();
    loop {
        let completed = events[start..].iter().filter(|e| matches!(&e.kind,
            EventKind::Provider(ProviderEvent::RequestCompleted { run_id:Some(id), .. }) if id == &lane.to_string())).count();
        let phase =
            events[start..].iter().rev().find_map(|e| match &e.kind {
                EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
                    run_id, to, ..
                }) if run_id == &lane.to_string() => Some(*to),
                _ => None,
            });
        if completed == 3 && phase == Some(AgentRunPhase::Waiting) {
            break;
        }
        let event = h.receiver.recv().await.unwrap();
        if let EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
            run_id,
            to: AgentRunPhase::Error,
            ..
        }) = &event.kind
        {
            assert_ne!(run_id, &lane.to_string(), "lane failed: {event:?}");
        }
        events.push(event);
    }
    let answered = h.runtime.user_answers(child).unwrap().remove(0);
    assert_eq!(answered.id, id);
    assert_eq!(answered.answer.as_deref(), Some("lead decision"));
    assert!(matches!(
        answered.answer_origin,
        Some(event_bus::AnswerOrigin::LaneLead { .. })
    ));
    assert!(
        attention(&config).is_empty(),
        "the original question is resolved"
    );
    // Continue the same histories across the Phase 2Q/B/C boundary. Each
    // externally driven step is ordered by a lifecycle or an explicit tool gate.
    host.held.store(true, Ordering::SeqCst);
    let lane_id = result(&h, "forward-open")["lane_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let observation = storage::lane_control::ObserveLaneStall {
        lane_id: lane_id.clone(),
        management_revision: 1,
        owner: delivery_owner,
        progress_anchor: db.handle().lane_progress_anchor(lane_id.clone()).unwrap(),
        classification: event_bus::lane::LaneStallClassification::NoReport,
        at_ns: 100,
    };
    let episode = db.handle().observe_lane_stall(observation.clone()).unwrap();
    db.handle()
        .request_lane_nudge(observation, episode.episode_id, episode.revision)
        .unwrap();
    assert!(
        attention(&config)
            .iter()
            .any(|i| matches!(i.kind, event_bus::lane::LaneAttentionKind::Stall { .. }))
    );
    h.mock
        .enqueue([text_response("nudge observed; prepare actual artifact")]);
    host.held.store(false, Ordering::SeqCst);
    h.runtime.drive_lane_deliveries(&thread).await.unwrap();
    events.extend(through_phase(&mut h.receiver, lane, AgentRunPhase::Waiting).await);
    host.held.store(true, Ordering::SeqCst);
    use sha2::{Digest, Sha256};
    let artifact = h._directory.path().join("integrated-artifact.txt");
    std::fs::write(&artifact, b"actual artifact bytes").unwrap();
    let digest = format!("{:x}", Sha256::digest(b"actual artifact bytes"));
    input(&mut h, lane, "report the nudged work ready", vec![
        call("journey-ready", "lane_report", json!({"status":"ready","summary":"independently reviewable", "artifact_revision":{"kind":"sha256","value":digest},"artifact_refs":[artifact.to_string_lossy()]})),
        text_response("ready reported"),
    ], &mut events).await;
    let ready = result(&h, "journey-ready");
    assert!(attention(&config).iter().any(|i| matches!(
        i.kind,
        event_bus::lane::LaneAttentionKind::ReadyUnclosed { .. }
    )));
    assert!(
        !attention(&config)
            .iter()
            .any(|i| matches!(i.kind, event_bus::lane::LaneAttentionKind::Stall { .. }))
    );
    let ready_seq = db
        .handle()
        .lane(&lane_id)
        .unwrap()
        .unwrap()
        .latest_report_seq;
    h.mock
        .enqueue([text_response("lead received ready reference")]);
    host.held.store(false, Ordering::SeqCst);
    h.runtime
        .drive_lane_deliveries("forward-cache-lead")
        .await
        .unwrap();
    events.extend(through_phase(&mut h.receiver, lead, AgentRunPhase::Waiting).await);
    host.held.store(true, Ordering::SeqCst);
    h.mock.enqueue([
        call(
            "journey-verify",
            "lane_verify",
            json!({"lane_id":lane_id,"ready_seq":ready_seq}),
        ),
        call("journey-gate", "read", json!({"index":0})),
        call("journey-gate", "read", json!({"index":0})),
    ]);
    h.runtime
        .send_message(lead, "request independent verification".into())
        .unwrap();
    let a = reads.recv().await.unwrap();
    let started_verification = result(&h, "journey-verify");
    assert!(
        started_verification["verification_ref"].is_string(),
        "verify failed before Reviewer startup: {started_verification}"
    );
    let b = reads.recv().await.unwrap();
    let (lead_gate, reviewer_gate) = if a.0 == lead.to_string() {
        (a.1, b)
    } else {
        (b.1, a)
    };
    let verified = result(&h, "journey-verify");
    assert!(attention(&config).iter().any(|i| matches!(
        i.kind,
        event_bus::lane::LaneAttentionKind::ReadyUnclosed {
            readiness: event_bus::lane::LaneReadyState::ReviewInProgress,
            ..
        }
    )));
    let reference = verified["verification_ref"].as_str().unwrap().to_owned();
    let reviewer: RunId = verified["reviewer_run_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(reviewer_gate.0, reviewer.to_string());
    h.mock.enqueue([text_response("lead awaits Reviewer")]);
    lead_gate.send(()).unwrap();
    events.extend(through_phase(&mut h.receiver, lead, AgentRunPhase::Waiting).await);
    h.mock.enqueue([
        call("journey-submit", "submit_lane_verification", json!({"verification_ref":reference,"verdict":"approved","checks":[{"criterion_index":0,"passed":true,"evidence_refs":[artifact.to_string_lossy()]}],"evidence_refs":[artifact.to_string_lossy()],"findings":[]})),
        text_response("lead observed independent Reviewer result"),
    ]);
    reviewer_gate.1.send(()).unwrap();
    h.runtime.wait(reviewer).await.unwrap();
    events.extend(through_phase(&mut h.receiver, lead, AgentRunPhase::Waiting).await);
    input(
        &mut h,
        lead,
        "close the verified lane",
        vec![
            call(
                "journey-close",
                "lane_close",
                json!({
                    "lane_id":lane_id,"ready_seq":ready_seq,
                    "expected_management_revision":verified["management_revision"],
                    "expected_request_revision":verified["request_revision"],
                    "note":"reviewed and accepted by the lead",
                    "verification_ref":reference
                }),
            ),
            text_response("closed with evidence"),
        ],
        &mut events,
    )
    .await;
    assert_eq!(result(&h, "journey-close")["outcome"], "closed");
    assert!(
        attention(&config).is_empty(),
        "independent verification and close resolve attention"
    );
    input(
        &mut h,
        lead,
        "continue after close",
        vec![text_response("prefix reusable after close")],
        &mut events,
    )
    .await;
    assert_eq!(
        result(&h, "journey-ready"),
        ready,
        "ready tool output was not rewritten by close"
    );
    assert_eq!(
        result(&h, "journey-verify"),
        verified,
        "Reviewer completion never rewrites verify output"
    );
    let requests = h
        .mock
        .recorded_requests()
        .into_iter()
        .filter(|r| r.path == Wire::ChatCompletions.path())
        .map(|r| r.body)
        .collect::<Vec<_>>();
    let child_requests = requests
        .iter()
        .filter(|r| actor(r) == 2)
        .collect::<Vec<_>>();
    assert!(
        child_requests.last().unwrap()["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["role"] == "tool"
                && m["tool_call_id"] == "return-answers"
                && m["content"].as_str().unwrap().contains(&id))
    );
    assert!(requests.iter().filter(|r| actor(r) == 1).any(|r| {
        r["messages"].as_array().unwrap().iter().any(|m| {
            m["content"]
                .as_str()
                .is_some_and(|s| s.contains("lane notifications") && s.contains(&id))
        })
    }));
    for who in 0..4 {
        let trace = requests
            .iter()
            .filter(|r| actor(r) == who)
            .collect::<Vec<_>>();
        for pair in trace.windows(2) {
            assert_append_only(CacheProtocol::OpenAi, pair[0], pair[1]).unwrap();
        }
    }
    assert_eq!(
        result(&h, "forward"),
        forwarded,
        "already sent forwarding result stays immutable"
    );
    assert_eq!(
        result(&h, "child-ask"),
        asked,
        "already sent ask result stays immutable"
    );
    for run in [lead, lane, child] {
        let usage = events
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
            .collect::<Vec<_>>();
        for pair in usage.windows(2) {
            assert!(pair[0].0 > 0);
            assert!(
                pair[1].1 >= pair[0].0,
                "input-derived cache must reuse the pre-forward/pre-restore prefix"
            );
        }
    }
    assert_eq!(h.mock.remaining_scripts(), 0);
    for run in [lane, lead] {
        h.runtime.cancel(run).unwrap();
        h.runtime.wait(run).await.unwrap();
    }
}
