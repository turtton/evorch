//! End-of-work reconciliation never wakes the model or edits sent history.
//! The next human-requested turn can read the authoritative interrupted plan.
use super::plan_cache::{add, read, result, store, update};

async fn send_turn(h: &mut Harness, run: RunId, script: Vec<ScriptedResponse>) -> Vec<Event> {
    h.mock.enqueue(script);
    h.runtime
        .send_message(run, "Next requested turn".into())
        .unwrap();
    through_phase(&mut h.receiver, run, AgentRunPhase::Waiting).await
}

#[tokio::test]
async fn a_lead_and_its_lane_read_only_their_own_interrupted_plan_and_reuse_their_prefix() {
    let mut h = harness(vec![text_response("lead idle")], 1_000_000);
    let (_config, _db) = store(&mut h);
    let lead = h
        .runtime
        .delegate_chat(
            "plan-lead",
            Role::Orchestrator,
            "Lead task".into(),
            RunConfig {
                conversation: true,
                interactive: true,
                keep_alive: true,
                ..Default::default()
            },
        )
        .unwrap();
    let mut events = through_phase(&mut h.receiver, lead, AgentRunPhase::Waiting).await;
    events.extend(send_turn(&mut h,lead,vec![
        call("open-lane","lane_open",json!({"brief":{"objective":"Independent lane","inputs":[],"deliverables":["artifact"],"acceptance_criteria":["independent review"],"constraints":[],"workspace":{},"report_policy":"attached"}})),
        text_response("lead idle"),text_response("lane idle"),
    ]).await);
    let lane = h
        .runtime
        .list_agents()
        .into_iter()
        .find(|row| row.parent_run_id.is_none() && row.run_id != lead)
        .unwrap()
        .run_id;
    if !events.iter().any(|event|matches!(&event.kind,
        EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {run_id,to:AgentRunPhase::Waiting,..}) if run_id==&lane.to_string())) {
        events.extend(through_phase(&mut h.receiver,lane,AgentRunPhase::Waiting).await);
    }
    for (run, title) in [(lead, "Lead coordination"), (lane, "Lane delivery")] {
        events.extend(
            send_turn(
                &mut h,
                run,
                vec![
                    update(title, 0, json!([add(title, "in_progress")])),
                    text_response("planned"),
                ],
            )
            .await,
        );
    }
    let requests = chat_requests(&h);
    let lane_thread = result(
        requests
            .iter()
            .rev()
            .find(|r| r["messages"].to_string().contains("open-lane"))
            .unwrap(),
        "open-lane",
    )["thread_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let lead_plan = h
        .runtime
        .conversation_plan_tree("plan-lead", None, None)
        .unwrap();
    let lane_plan = h
        .runtime
        .conversation_plan_tree(&lane_thread, None, None)
        .unwrap();
    assert_ne!(lead_plan.plan_id, lane_plan.plan_id);
    for plan in [&lead_plan, &lane_plan] {
        assert_eq!(plan.revision, 2);
        assert_eq!(
            plan.nodes[0].status,
            Some(event_bus::conversation_plan::PlanTaskStatus::Interrupted)
        );
    }
    let before = requests.len();
    for run in [lead, lane] {
        events.extend(
            send_turn(
                &mut h,
                run,
                vec![read("marked"), read_response(2), text_response("ready")],
            )
            .await,
        );
    }
    let requests = chat_requests(&h);
    assert_eq!(
        requests.len(),
        before + 6,
        "reconciliation never causes an extra request"
    );
    let is_lead = |r: &Value| {
        r["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["function"]["name"] == "lane_open")
    };
    for (run, expected) in [(lead, &lead_plan), (lane, &lane_plan)] {
        let group = requests
            .iter()
            .filter(|r| is_lead(r) == (run == lead))
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(
            result(group.last().unwrap(), "marked")["nodes"],
            serde_json::to_value(&expected.nodes).unwrap()
        );
        let other = if run == lead {
            "Lane delivery"
        } else {
            "Lead coordination"
        };
        assert!(
            group
                .iter()
                .all(|r| !r["messages"].to_string().contains(other)),
            "another root's plan leaked"
        );
        super::plan_cache::verify_root_prefix(&h, run, &events, &group);
        h.runtime.cancel(run).unwrap();
        h.runtime.wait(run).await.unwrap();
    }
    assert_eq!(h.mock.remaining_scripts(), 0);
}
use super::*;

#[tokio::test]
async fn plan_add_update_and_a_finished_turn_keep_the_wire_prefix() {
    let mut h = harness(
        vec![
            update(
                "add-and-update",
                0,
                json!([
                    add("Implementation","pending"),add("Verification","pending"),
                    {"op":"update","task":{"ref":"Implementation"},"status":"in_progress"},
                    {"op":"update","task":{"ref":"Verification"},"status":"in_progress"}
                ]),
            ),
            read("running"),
            read_response(0),
            text_response("first turn ended"),
            read("interrupted"),
            text_response("second turn"),
        ],
        1_000_000,
    );
    let (_config, _db) = store(&mut h);
    let run = h
        .runtime
        .delegate_chat(
            "plan-boundary",
            Role::Worker,
            "Inspect and verify changes".into(),
            RunConfig {
                conversation: true,
                interactive: true,
                keep_alive: true,
                ..Default::default()
            },
        )
        .unwrap();
    let mut events = through_phase(&mut h.receiver, run, AgentRunPhase::Waiting).await;
    let marked = h
        .runtime
        .conversation_plan_tree("plan-boundary", None, None)
        .unwrap();
    assert_eq!(marked.revision, 2);
    assert!(
        marked
            .nodes
            .iter()
            .all(|n| n.status == Some(event_bus::conversation_plan::PlanTaskStatus::Interrupted))
    );
    assert_eq!(
        chat_requests(&h).len(),
        4,
        "reconciliation makes no provider request"
    );
    h.runtime
        .send_message(run, "Continue after the interruption".into())
        .unwrap();
    events.extend(through_phase(&mut h.receiver, run, AgentRunPhase::Waiting).await);
    h.runtime.stop(run, runtime::StopScope::SelfOnly).unwrap();
    events.extend(through_phase(&mut h.receiver, run, AgentRunPhase::Stopped).await);
    assert_eq!(
        h.runtime
            .conversation_plan_tree("plan-boundary", None, None)
            .unwrap(),
        marked
    );
    let requests = chat_requests(&h);
    assert_eq!(requests.len(), 6);
    assert_eq!(result(&requests[2], "running")["revision"], 1);
    let actual = result(&requests[5], "interrupted");
    assert_eq!(actual["revision"], 2);
    assert_eq!(actual["nodes"], serde_json::to_value(marked.nodes).unwrap());
    verify_trace(&h, run, &events, 0);
}

#[tokio::test]
async fn an_interrupted_plan_survives_manual_compaction_then_reuses_the_wire_prefix() {
    let review = support::ReviewStorage::new();
    let mut h = harness(
        vec![
            update("running", 0, json!([add("Active task", "in_progress")])),
            text_response(&"old analysis that can be summarized ".repeat(100)),
            read("marked"),
            read_response(2),
            text_response("done"),
        ],
        1_000_000,
    );
    let (_config, _db) = store(&mut h);
    let run = h
        .runtime
        .delegate_chat(
            "marked-plan",
            Role::Worker,
            "Inspect changes".into(),
            RunConfig {
                conversation: true,
                interactive: true,
                ..Default::default()
            },
        )
        .unwrap();
    let mut events = through_phase(&mut h.receiver, run, AgentRunPhase::Waiting).await;
    let marked = h
        .runtime
        .conversation_plan_tree("marked-plan", None, None)
        .unwrap();
    assert_eq!(
        marked.nodes[0].status,
        Some(event_bus::conversation_plan::PlanTaskStatus::Interrupted)
    );
    h.runtime.compact(run).unwrap();
    review.attach(&h.runtime);
    h.runtime
        .send_message(run, "Continue after the context boundary".into())
        .unwrap();
    events.extend(through_phase(&mut h.receiver, run, AgentRunPhase::Done).await);
    let requests = chat_requests(&h);
    assert_eq!(
        result(requests.last().unwrap(), "marked")["nodes"],
        serde_json::to_value(marked.nodes).unwrap()
    );
    verify_trace(&h, run, &events, 1);
}
