//! Procedure state and lane delivery stay independent: a notification resumes a
//! root like any input and its answer ends the work period normally, while
//! neither a procedure change nor a notification that is only held makes a
//! request, wakes anyone, or marks anything.
use super::*;
use event_bus::{ThreadTodoItem, ThreadTodoStatus};

fn call(name: &str, id: &str, input: serde_json::Value) -> ContentBlock {
    ContentBlock::ToolUse {
        id: id.into(),
        name: name.into(),
        input,
    }
}

fn text(messages: &[Message]) -> String {
    serde_json::to_string(messages).unwrap()
}

fn snapshots(messages: &[Message], thread: &str) -> usize {
    messages
        .iter()
        .flat_map(|message| &message.content)
        .filter(|block| {
            matches!(block, ContentBlock::Text { text }
                if text.starts_with("[runtime conversation procedure state]")
                    && text.contains(thread))
        })
        .count()
}

#[tokio::test]
async fn a_notification_resumes_the_lead_and_its_answer_ends_the_period_normally() {
    let mut f = Fixture::new();
    let (lead, _host) = f.lead().await;
    let lane = f.lane(lead, "resumed-lane").await;
    let id = lead.to_string();

    // The lead plans, and its finished turn marks the running item.
    f.model.next_tools.lock().unwrap().insert(
        id.clone(),
        call(
            "todo_write",
            "plan",
            serde_json::json!({"items":[{"content":"Coordinate lanes","status":"in_progress"}]}),
        ),
    );
    f.runtime
        .send_message(lead, "plan the work".into())
        .unwrap();
    f.phase(lead, AgentRunPhase::Waiting).await;
    let planned = f.runtime.thread_todo("delivery-lead").unwrap();
    assert_eq!(planned.revision, 2);
    assert_eq!(planned.items[0].status, ThreadTodoStatus::Interrupted);

    // A ready report wakes the lead; it resumes the item itself and ends its
    // turn, which marks it again exactly once.
    f.model.next_tools.lock().unwrap().insert(
        id,
        call(
            "todo_write",
            "resume",
            serde_json::json!({"update":{"index":0,"status":"in_progress"},"expected_revision":2}),
        ),
    );
    let before = f.model.requests.lock().unwrap().len();
    f.report(&lane, Some(1), LaneStatus::Ready);
    assert_eq!(
        f.drive().await,
        LaneDeliveryOutcome::Delivered { notifications: 1 }
    );
    f.phase(lead, AgentRunPhase::Waiting).await;
    let requests = f.model.requests.lock().unwrap();
    assert_eq!(
        requests.len(),
        before + 2,
        "the resumed turn and nothing else"
    );
    assert!(text(&requests[before]).contains(&lane.lane_id));
    // The earlier mark is presented once with the notification, not repeated.
    assert_eq!(snapshots(&requests[before], "delivery-lead"), 1);
    assert_eq!(snapshots(&requests[before + 1], "delivery-lead"), 1);
    let ended = f.runtime.thread_todo("delivery-lead").unwrap();
    assert_eq!(ended.revision, 4, "model update, then one mark");
    assert_eq!(ended.items[0].status, ThreadTodoStatus::Interrupted);
    assert_eq!(ended.items.len(), 1);
}

#[tokio::test]
async fn procedure_changes_cause_no_request_wake_delivery_or_mark() {
    let mut f = Fixture::new();
    let (lead, host) = f.lead().await;
    let lane = f.lane(lead, "quiet-lane").await;
    let root = lead;
    // A list that is running while the lead waits (as when a question is open).
    f.runtime
        .write_thread_todo(
            root,
            crate::thread_todos::Operation::Replace(vec![ThreadTodoItem {
                content: "Coordinate lanes".into(),
                status: ThreadTodoStatus::InProgress,
            }]),
        )
        .unwrap();
    let before = f.model.requests.lock().unwrap().len();
    let restores = host.restores.load(Ordering::SeqCst);

    // Edits to a running list resume nobody and deliver nothing.
    f.runtime
        .write_thread_todo(
            root,
            crate::thread_todos::Operation::Update {
                index: 0,
                status: ThreadTodoStatus::Pending,
                expected_revision: 1,
            },
        )
        .unwrap();
    f.runtime
        .write_thread_todo(
            root,
            crate::thread_todos::Operation::Update {
                index: 0,
                status: ThreadTodoStatus::InProgress,
                expected_revision: 2,
            },
        )
        .unwrap();
    assert_eq!(f.model.requests.lock().unwrap().len(), before);
    assert_eq!(host.restores.load(Ordering::SeqCst), restores);
    assert_eq!(f.receipts(), 0);
    assert_eq!(f.drive().await, LaneDeliveryOutcome::Empty);
    assert_eq!(
        f.runtime.inspect_agent(lane.root_run_id).unwrap().phase,
        AgentRunPhase::Waiting
    );
    let list = f.runtime.thread_todo("delivery-lead").unwrap();
    assert_eq!(list.revision, 3);
    assert_eq!(list.items[0].status, ThreadTodoStatus::InProgress);
    assert_eq!(
        f.runtime.inspect_agent(lead).unwrap().phase,
        AgentRunPhase::Waiting
    );
}

#[tokio::test]
async fn a_stopped_lead_is_marked_once_and_a_durable_stop_keeps_it_unresumed() {
    let mut f = Fixture::new();
    let (lead, _host) = f.lead().await;
    let lane = f.lane(lead, "stopped-lane").await;
    f.runtime
        .write_thread_todo(
            lead,
            crate::thread_todos::Operation::Replace(vec![ThreadTodoItem {
                content: "Coordinate lanes".into(),
                status: ThreadTodoStatus::InProgress,
            }]),
        )
        .unwrap();
    let before = f.model.requests.lock().unwrap().len();

    f.runtime.stop(lead, crate::StopScope::SelfOnly).unwrap();
    f.runtime.wait(lead).await.unwrap();
    let stopped = f.runtime.thread_todo("delivery-lead").unwrap();
    assert_eq!(stopped.revision, 2, "one mark when the work period ended");
    assert_eq!(stopped.items[0].status, ThreadTodoStatus::Interrupted);
    assert!(
        f.storage
            .handle()
            .lane_delivery_destination("delivery-lead".into())
            .unwrap()
            .unwrap()
            .stopped
    );

    // Neither a later report nor an explicit-resume refusal touches the list,
    // and the stopped lead is not woken to read the notification.
    f.report(&lane, Some(1), LaneStatus::Ready);
    let outcome = f.drive().await;
    assert!(
        !matches!(outcome, LaneDeliveryOutcome::Delivered { .. }),
        "{outcome:?}"
    );
    assert!(
        f.runtime
            .resume_lane_deliveries("delivery-lead", lead, None)
            .is_err()
    );
    assert_eq!(f.runtime.thread_todo("delivery-lead").unwrap(), stopped);
    assert_eq!(f.model.requests.lock().unwrap().len(), before);
    assert_eq!(
        f.runtime.inspect_agent(lead).unwrap().phase,
        AgentRunPhase::Stopped
    );
}
