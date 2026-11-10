//! A lead's supplements and brief revisions reach the lane through the real outbox:
//! whole, in order, once, and without disturbing the lane's provider-visible prefix.
use super::nudge::NudgeFixture;
use super::*;
use storage::lane_instructions::LaneInstructionDelivery;

fn lead_run(n: &NudgeFixture) -> RunId {
    n.f.store
        .handle()
        .lane(&n.observation.lane_id)
        .unwrap()
        .unwrap()
        .origin_run_id
        .parse()
        .unwrap()
}
fn send(id: &str, lane_id: &str, management: u64, brief: u64, message: &str) -> ScriptedResponse {
    call(
        id,
        "lane_send",
        json!({"operation_id":id,"lane_id":lane_id,"expected_management_revision":management,
            "expected_brief_revision":brief,"message":message}),
    )
}
fn revise(
    id: &str,
    lane_id: &str,
    management: u64,
    brief: u64,
    objective: &str,
) -> ScriptedResponse {
    call(
        id,
        "lane_revise_brief",
        json!({"operation_id":id,"lane_id":lane_id,"expected_management_revision":management,
            "expected_brief_revision":brief,"brief":super::super::brief(objective)}),
    )
}
async fn lead_says(n: &mut NudgeFixture, lead: RunId, scripts: Vec<ScriptedResponse>) {
    input(
        &mut n.f.h,
        lead,
        "instruct the lane",
        scripts,
        &mut n.f.events,
    )
    .await;
}
fn lane_text(request: &Value) -> String {
    request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["content"].as_str().unwrap_or("").to_owned())
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn instructions_accepted_before_detach_arrive_in_order_once_and_keep_the_wire_prefix() {
    let mut n = NudgeFixture::new().await;
    let lead = lead_run(&n);
    let lane_id = n.observation.lane_id.clone();
    // Deny only the automatic trigger: the explicit driver is the completion barrier.
    n.f.host.denied.store(true, Ordering::SeqCst);
    lead_says(
        &mut n,
        lead,
        vec![
            send("send-1", &lane_id, 1, 1, "prefer the small fix"),
            revise("revise-1", &lane_id, 1, 1, "Reworked objective"),
            call(
                "detach",
                "lane_detach",
                json!({"operation_id":"detach-1","lane_id":lane_id,"expected_management_revision":1}),
            ),
            send("late-1", &lane_id, 2, 2, "from a lead that no longer manages the lane"),
            text_response("instructed"),
        ],
    )
    .await;
    let first = result(&n.f.h, "send-1");
    assert_eq!(first["status"], "accepted");
    assert_eq!(first["delivery"], "queued");
    assert_eq!(first["operation"]["instruction_seq"], 1);
    assert_eq!(first["operation"]["brief_revision"], 1);
    let second = result(&n.f.h, "revise-1");
    assert_eq!(second["operation"]["instruction_seq"], 2);
    assert_eq!(second["operation"]["brief_revision"], 2);
    assert_eq!(
        result(&n.f.h, "detach")["current_lane"]["policy"],
        "detached"
    );
    let late: Value = result(&n.f.h, "late-1");
    assert_eq!(late["code"], "not_current_lead");
    assert_eq!(n.receipts(), 0);

    n.f.host.checked.notified().await;
    n.f.host.denied.store(false, Ordering::SeqCst);
    for (reply, expected_seq) in [
        ("lane read the supplement", 1),
        ("lane read the revision", 2),
    ] {
        n.f.h.mock.enqueue(vec![text_response(reply)]);
        assert_eq!(
            n.drive().await.unwrap(),
            LaneDeliveryOutcome::Delivered { notifications: 1 }
        );
        n.f.delivered_request().await;
        assert_eq!(
            n.f.store
                .handle()
                .lane_instruction_delivery(&lane_id, expected_seq)
                .unwrap(),
            Some(LaneInstructionDelivery::Delivered)
        );
    }
    assert_eq!(n.drive().await.unwrap(), LaneDeliveryOutcome::Empty);
    assert_eq!(n.receipts(), 2);
    n.f.followup("ordinary turn after both instructions").await;

    let requests = n.f.requests();
    let text = lane_text(requests.last().unwrap());
    assert_eq!(text.matches("prefer the small fix").count(), 1);
    assert_eq!(text.matches("Reworked objective").count(), 1);
    assert!(text.find("prefer the small fix") < text.find("Reworked objective"));
    assert_eq!(text.matches("Agent-authored instruction").count(), 2);
    assert!(!text.contains("no longer manages the lane"));
    n.f.verify(0);

    let tool_names = |request: &Value| -> Vec<String> {
        request["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap().to_owned())
            .collect()
    };
    assert!(
        requests
            .iter()
            .all(|r| !tool_names(r).contains(&"lane_send".to_owned()))
    );
    let lead_tools: Vec<_> =
        n.f.h
            .mock
            .recorded_requests()
            .iter()
            .filter(|r| r.path == Wire::ChatCompletions.path())
            .map(|r| tool_names(&r.body))
            .filter(|names| names.contains(&"lane_send".to_owned()))
            .collect();
    assert!(lead_tools.len() >= 2);
    assert!(
        lead_tools.windows(2).all(|pair| pair[0] == pair[1]),
        "the lead's tool schema never changes with lane state"
    );
    n.cancel().await;
}

#[tokio::test]
async fn a_finished_lane_is_woken_for_an_instruction_and_receives_it_once() {
    let mut n = NudgeFixture::new().await;
    let lead = lead_run(&n);
    let lane_id = n.observation.lane_id.clone();
    n.f.finish().await;
    n.f.host.denied.store(true, Ordering::SeqCst);
    lead_says(
        &mut n,
        lead,
        vec![
            send("wake-send", &lane_id, 1, 1, "rerun with the new constraint"),
            text_response("sent"),
        ],
    )
    .await;
    n.f.host.checked.notified().await;
    n.f.host.denied.store(false, Ordering::SeqCst);
    n.f.h.mock.enqueue(vec![call(
        "woken-finish",
        "finish",
        json!({"result":"handled the instruction"}),
    )]);
    assert_eq!(
        n.drive().await.unwrap(),
        LaneDeliveryOutcome::Delivered { notifications: 1 }
    );
    n.f.events
        .extend(through_phase(&mut n.f.h.receiver, n.f.lead, AgentRunPhase::Done).await);
    n.f.h.runtime.wait(n.f.lead).await.unwrap();
    assert_eq!(n.f.host.restores.load(Ordering::SeqCst), 1);
    assert_eq!(n.receipts(), 1);
    assert_eq!(n.drive().await.unwrap(), LaneDeliveryOutcome::Empty);
    assert_eq!(n.f.host.restores.load(Ordering::SeqCst), 1);
    assert_eq!(
        n.f.store
            .handle()
            .lane_instruction_delivery(&lane_id, 1)
            .unwrap(),
        Some(LaneInstructionDelivery::Delivered)
    );
}

#[tokio::test]
async fn a_stopped_lane_keeps_the_instruction_pending_until_an_explicit_resume() {
    let mut n = NudgeFixture::new().await;
    let lead = lead_run(&n);
    let lane_id = n.observation.lane_id.clone();
    n.stopped().await;
    lead_says(
        &mut n,
        lead,
        vec![
            send("stop-send", &lane_id, 1, 1, "hold until resumed"),
            text_response("sent"),
        ],
    )
    .await;
    assert!(matches!(
        n.drive().await.unwrap(),
        LaneDeliveryOutcome::Stopped
            | LaneDeliveryOutcome::Nacked(storage::lane_delivery::LaneDeliveryNack::Stopped)
    ));
    assert_eq!(n.receipts(), 0);
    assert_eq!(
        n.f.store
            .handle()
            .lane_instruction_delivery(&lane_id, 1)
            .unwrap(),
        Some(LaneInstructionDelivery::Queued)
    );
    n.f.h
        .mock
        .enqueue(vec![text_response("read after the explicit resume")]);
    n.f.h
        .runtime
        .resume_chat(n.f.lead, n.f.host.target.authority.clone())
        .unwrap();
    n.f.delivered_request().await;
    assert_eq!(n.receipts(), 1);
    assert_eq!(n.drive().await.unwrap(), LaneDeliveryOutcome::Empty);
    assert_eq!(n.receipts(), 1);
    n.cancel().await;
}
