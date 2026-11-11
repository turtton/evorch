//! A lead's supplements and brief revisions reach the lane through the real outbox:
//! whole, in order, once, and without disturbing the lane's provider-visible prefix.
use super::nudge::NudgeFixture;
use super::recovery::FailingHost;
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

struct ReleaseOnDrop(mock_openai::ResponseGate);
impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        self.0.release();
    }
}
fn ready(summary: &str) -> Value {
    json!({"status":"ready","summary":summary,"artifact_refs":[]})
}
fn report_revisions(n: &NudgeFixture) -> Vec<u64> {
    n.f.store
        .handle()
        .lane_reports(&n.observation.lane_id, 0, 20)
        .unwrap()
        .iter()
        .map(|report| report.brief_revision)
        .collect()
}

/// `flush_lane_deliveries` runs only between provider requests (turn start and the idle
/// wait), never while a response's tools execute. So at `lane_report` dispatch the
/// delivery receipts are exactly the instructions the answering model's context holds.
/// The gate keeps the response in flight across the claim; it must stay bound to the
/// revision that model saw, and only the next turn may observe (and report) the new one.
#[tokio::test]
async fn a_ready_from_a_response_started_before_delivery_stays_bound_to_the_revision_the_lane_saw()
{
    let mut n = NudgeFixture::new().await;
    let lead = lead_run(&n);
    let lane_id = n.observation.lane_id.clone();
    n.f.host.denied.store(true, Ordering::SeqCst);
    lead_says(
        &mut n,
        lead,
        vec![
            revise("rev-1", &lane_id, 1, 1, "Reworked objective"),
            text_response("revised"),
        ],
    )
    .await;
    n.f.host.checked.notified().await;

    let (gate, arrived) = mock_openai::ResponseGate::new();
    let _release = ReleaseOnDrop(gate.clone());
    n.f.h.mock.enqueue(vec![
        call(
            "late-ready",
            "lane_report",
            ready("finished before reading the revision"),
        )
        .with_gate(gate.clone()),
        text_response("reported"),
    ]);
    // Input to the lane schedules a delivery too; the denied host answers it, so the
    // instruction is still unclaimed when the lane's request goes out.
    n.f.h
        .runtime
        .send_message(n.f.lead, "report when finished".into())
        .unwrap();
    n.f.host.checked.notified().await;
    tokio::task::spawn_blocking(move || arrived.recv().unwrap())
        .await
        .unwrap();
    assert_eq!(n.receipts(), 0);
    // The lane's request is on the wire. Claim the instruction now; its flush can only
    // happen after this response's tools have run.
    n.f.host.denied.store(false, Ordering::SeqCst);
    let runtime = n.f.h.runtime.clone();
    let thread = n.f.host.thread.clone();
    let drive = tokio::spawn(async move { runtime.drive_lane_deliveries(&thread).await });
    n.f.host.checked.notified().await;
    gate.release();
    assert_eq!(
        drive.await.unwrap().unwrap(),
        LaneDeliveryOutcome::Delivered { notifications: 1 }
    );
    n.f.events
        .extend(through_phase(&mut n.f.h.receiver, n.f.lead, AgentRunPhase::Waiting).await);
    assert_eq!(report_revisions(&n), [1]);
    assert_eq!(
        n.f.store
            .handle()
            .lane(&lane_id)
            .unwrap()
            .unwrap()
            .brief_revision,
        2
    );

    input(
        &mut n.f.h,
        n.f.lead,
        "report again with the revision in context",
        vec![
            call(
                "fresh-ready",
                "lane_report",
                ready("finished the revised brief"),
            ),
            text_response("reported again"),
        ],
        &mut n.f.events,
    )
    .await;
    assert_eq!(report_revisions(&n), [1, 2]);
    n.cancel().await;
}

async fn lane_reports_ready(n: &mut NudgeFixture, call_id: &str, summary: &str) {
    input(
        &mut n.f.h,
        n.f.lead,
        "report the current state",
        vec![
            call(call_id, "lane_report", ready(summary)),
            text_response("reported"),
        ],
        &mut n.f.events,
    )
    .await;
}
fn current_brief_revision(n: &NudgeFixture) -> u64 {
    n.f.store
        .handle()
        .lane(&n.observation.lane_id)
        .unwrap()
        .unwrap()
        .brief_revision
}
fn instruction_state(n: &NudgeFixture) -> String {
    rusqlite::Connection::open(&n.f.config.db_path)
        .unwrap()
        .query_row(
            "SELECT n.state FROM lane_instructions i JOIN lane_notifications n ON n.notification_id=i.notification_id WHERE i.instruction_seq=1",
            [],
            |row| row.get(0),
        )
        .unwrap()
}

/// A stop while the response is in flight abandons it. The instruction is then delivered
/// by the resumed lane, and the late answer of the abandoned request can no longer be
/// recorded: the lane's only report is the one made with the revision in its context.
#[tokio::test]
async fn an_abandoned_response_cannot_report_after_the_revision_was_delivered() {
    let mut n = NudgeFixture::new().await;
    let lead = lead_run(&n);
    let lane_id = n.observation.lane_id.clone();
    n.f.host.denied.store(true, Ordering::SeqCst);
    lead_says(
        &mut n,
        lead,
        vec![
            revise("rev-1", &lane_id, 1, 1, "Reworked objective"),
            text_response("revised"),
        ],
    )
    .await;
    n.f.host.checked.notified().await;

    let (gate, arrived) = mock_openai::ResponseGate::new();
    let _release = ReleaseOnDrop(gate.clone());
    n.f.h.mock.enqueue(vec![
        call(
            "abandoned-ready",
            "lane_report",
            ready("finished before reading the revision"),
        )
        .with_gate(gate.clone()),
    ]);
    n.f.h
        .runtime
        .send_message(n.f.lead, "report when finished".into())
        .unwrap();
    n.f.host.checked.notified().await;
    tokio::task::spawn_blocking(move || arrived.recv().unwrap())
        .await
        .unwrap();
    n.stopped().await;
    n.f.host.denied.store(false, Ordering::SeqCst);
    n.f.h
        .mock
        .enqueue(vec![text_response("read the revision after the resume")]);
    n.f.h
        .runtime
        .resume_chat(n.f.lead, n.f.host.target.authority.clone())
        .unwrap();
    n.f.delivered_request().await;
    assert_eq!(n.receipts(), 1);
    // The old response arrives only now, after the revision is in the conversation.
    gate.release();
    n.f.followup("anything late from the abandoned request?")
        .await;
    assert_eq!(report_revisions(&n), Vec::<u64>::new());

    lane_reports_ready(&mut n, "fresh-ready", "finished the revised brief").await;
    assert_eq!(report_revisions(&n), [2]);
    n.cancel().await;
}

/// The receipt commits with the conversation snapshot; the ACK is a later step. A
/// failure between them, and a restart after it, must neither deliver the instruction a
/// second time nor make the lane's next report claim the old revision.
#[tokio::test]
async fn an_instruction_received_but_not_acked_survives_restart_without_reinjection() {
    let mut n = NudgeFixture::new().await;
    let lead = lead_run(&n);
    let lane_id = n.observation.lane_id.clone();
    n.stopped().await;
    lead_says(
        &mut n,
        lead,
        vec![
            revise("rev-1", &lane_id, 1, 1, "Reworked objective"),
            text_response("revised"),
        ],
    )
    .await;
    assert_eq!(n.receipts(), 0);
    n.f.sql("CREATE TRIGGER fail_instruction_ack BEFORE UPDATE OF state ON lane_notifications WHEN NEW.state='acked' BEGIN SELECT RAISE(ABORT,'injected ACK failure'); END;");
    n.f.h
        .mock
        .enqueue(vec![text_response("read the revision after the resume")]);
    n.f.h
        .runtime
        .resume_chat(n.f.lead, n.f.host.target.authority.clone())
        .unwrap();
    n.f.delivered_request().await;
    n.next_failure(0).await;
    assert_eq!(n.receipts(), 1);
    assert_ne!(instruction_state(&n), "acked");
    assert_eq!(
        n.f.store
            .handle()
            .lane_instruction_delivery(&lane_id, 1)
            .unwrap(),
        Some(LaneInstructionDelivery::Delivered),
        "the receipt, not the lost ACK, proves delivery"
    );

    // Restart the lane: the snapshot holds the instruction, so nothing is delivered again.
    n.stopped().await;
    let failures = n.failures();
    n.f.h
        .runtime
        .resume_chat(n.f.lead, n.f.host.target.authority.clone())
        .unwrap();
    n.f.events
        .extend(through_phase(&mut n.f.h.receiver, n.f.lead, AgentRunPhase::Waiting).await);
    n.next_failure(failures).await;
    assert_eq!(n.receipts(), 1);
    lane_reports_ready(&mut n, "after-restart", "finished the revised brief").await;
    assert_eq!(report_revisions(&n), [2]);
    n.f.sql("DROP TRIGGER fail_instruction_ack");
    assert_eq!(n.drive().await.unwrap(), LaneDeliveryOutcome::Empty);
    assert_eq!(n.receipts(), 1);
    let text = lane_text(n.f.requests().last().unwrap());
    assert_eq!(text.matches("Reworked objective").count(), 1);
    n.cancel().await;
}

/// A compaction after delivery is the one history replacement; the delivered revision
/// stays delivered, is never queued again, and later reports are bound to it.
#[tokio::test]
async fn a_delivered_revision_is_not_redelivered_after_compaction_and_binds_later_reports() {
    let mut n = NudgeFixture::new().await;
    let lead = lead_run(&n);
    let lane_id = n.observation.lane_id.clone();
    n.f.host.denied.store(true, Ordering::SeqCst);
    lead_says(
        &mut n,
        lead,
        vec![
            revise("rev-1", &lane_id, 1, 1, "Reworked objective"),
            text_response("revised"),
        ],
    )
    .await;
    n.f.host.checked.notified().await;
    n.f.host.denied.store(false, Ordering::SeqCst);
    n.f.h.mock.enqueue(vec![text_response("read the revision")]);
    assert_eq!(
        n.drive().await.unwrap(),
        LaneDeliveryOutcome::Delivered { notifications: 1 }
    );
    n.f.delivered_request().await;
    input(
        &mut n.f.h,
        n.f.lead,
        "large old context",
        vec![read_response(1), text_response("large turn complete")],
        &mut n.f.events,
    )
    .await;
    n.stopped().await;
    n.f.h
        .runtime
        .compact_saved_chat(n.f.lead, n.f.host.target.authority.clone())
        .await
        .unwrap();
    loop {
        let e = n.f.h.receiver.recv().await.unwrap();
        let done = matches!(&e.kind,EventKind::Compaction(CompactionEvent::Compacted{run_id,..})if run_id==&n.f.lead.to_string());
        n.f.events.push(e);
        if done {
            break;
        }
    }
    n.f.h
        .runtime
        .resume_chat(n.f.lead, n.f.host.target.authority.clone())
        .unwrap();
    n.f.events
        .extend(through_phase(&mut n.f.h.receiver, n.f.lead, AgentRunPhase::Waiting).await);
    assert_eq!(n.drive().await.unwrap(), LaneDeliveryOutcome::Empty);
    assert_eq!(n.receipts(), 1);
    assert_eq!(
        n.f.store
            .handle()
            .lane_instruction_delivery(&lane_id, 1)
            .unwrap(),
        Some(LaneInstructionDelivery::Delivered)
    );
    lane_reports_ready(&mut n, "after-compaction", "finished the revised brief").await;
    assert_eq!(report_revisions(&n), [2]);
    n.f.followup("reuse the compacted prefix").await;
    n.f.verify(1);
    n.cancel().await;
}

/// A restore that keeps failing suppresses recovery. Until the explicit Resume the
/// revision stays queued (the lane's own finishing report is the only one, on the
/// revision it knew); the Resume then delivers it once.
#[tokio::test]
async fn an_instruction_held_by_suppressed_recovery_is_delivered_once_after_resume() {
    let mut n = NudgeFixture::new().await;
    let lead = lead_run(&n);
    let lane_id = n.observation.lane_id.clone();
    n.f.finish().await;
    let host = Arc::new(FailingHost {
        inner: n.f.host.clone(),
        fail: AtomicBool::new(true),
        attempts: AtomicUsize::new(0),
    });
    n.f.h.runtime.bind_lane_delivery_host(host.clone());
    n.f.host.denied.store(true, Ordering::SeqCst);
    lead_says(
        &mut n,
        lead,
        vec![
            revise("rev-1", &lane_id, 1, 1, "Reworked objective"),
            text_response("revised"),
        ],
    )
    .await;
    n.f.host.checked.notified().await;
    n.f.host.denied.store(false, Ordering::SeqCst);
    for _ in 0..6 {
        assert_eq!(
            n.drive().await.unwrap(),
            LaneDeliveryOutcome::Nacked(storage::lane_delivery::LaneDeliveryNack::RestoreFailed)
        );
    }
    assert_eq!(host.attempts.load(Ordering::SeqCst), 3);
    assert_eq!(n.receipts(), 0);
    assert_eq!(
        n.f.store
            .handle()
            .lane_instruction_delivery(&lane_id, 1)
            .unwrap(),
        Some(LaneInstructionDelivery::Queued)
    );
    assert_eq!(current_brief_revision(&n), 2);
    assert_eq!(report_revisions(&n), [1]);
    host.fail.store(false, Ordering::SeqCst);
    n.f.h
        .mock
        .enqueue(vec![text_response("read after the resume")]);
    n.f.h
        .runtime
        .resume_lane_deliveries(&n.f.host.thread, n.f.lead, None)
        .unwrap();
    n.f.delivered_request().await;
    assert_eq!(n.receipts(), 1);
    assert_eq!(n.drive().await.unwrap(), LaneDeliveryOutcome::Empty);
    assert_eq!(n.receipts(), 1);
    assert_eq!(current_brief_revision(&n), 2);
    lane_reports_ready(&mut n, "after-resume", "finished the revised brief").await;
    assert_eq!(report_revisions(&n), [1, 2]);
    n.cancel().await;
}
