use super::*;
use serde_json::json;

fn source(id: u64, parent: Option<u64>) -> RecoverySource {
    let run = RunId::new(id);
    let parent = parent.map(RunId::new);
    let intent = serde_json::from_value(json!({
        "version":1,"run_id":run,"incarnation":"old","parent_run_id":parent,"role":"worker",
        "wait":{"kind":"admission"},"initial_input":null,"inputs":[],"active":true,
        "resume_requested":true,"stop_reason":null,"checkpoint":null,"snapshot":null,
        "tool_intent":[],"provider_input":null,"boundary_pending":false,
        "responses_replay":crate::ResponsesReplayState::default(),"compactions":[],"route_pins":[]
    }))
    .unwrap();
    RecoverySource {
        record: serde_json::from_value(json!({
            "version":1,"run_id":run,"old_incarnation":"old","intent_revision":1,
            "parent":parent,"root":RunId::new(1),"thread":null,"project":null,"role":"worker",
            "recipe":{"settings_reference":"current","policy":{"owner":"core_owned"},
                "goal_id":null,"team":null,"lane":null,"descriptor":null},
            "waits":[],"deliveries":[],"completion":null,"auto_wakes":20,
            "user_reset_generation":9,"input_ids":[]
        }))
        .unwrap(),
        intent,
        bundle: None,
        snapshot: None,
    }
}

fn delivery(id: &str) -> DurableDelivery {
    DurableDelivery {
        message_id: id.into(),
        sender: RunId::new(2),
        recipient: RunId::new(1),
        sequence: 0,
        state: DurableDeliveryState::Mailbox,
        consumed_receipt: None,
        notice: DurableNotice::Completion {
            child: RunId::new(2),
            recipient: Some(RunId::new(1)),
            generation: 7,
            observed: false,
        },
        message: Some(AgentMessage {
            message_id: id.into(),
            sender_run_id: "run-2".into(),
            recipient_run_id: "run-1".into(),
            kind: AgentMessageKind::Send,
            content: "finished".into(),
            reply_to: None,
        }),
    }
}

#[test]
fn completion_queue_and_output_share_the_saved_generation_and_observation() {
    let mut root = source(1, None);
    root.record.deliveries = vec![delivery("msg-5")];
    let child = source(2, Some(1));
    let relations = PreparedRelations::prepare(&[root, child]).unwrap();
    let queued = &relations.queues[&RunId::new(1)][0].1;
    let notice = &relations.completions[&RunId::new(2)];
    let crate::mailbox::completion::MessageOrigin::CompletionNotice(queued_notice) = &queued.origin
    else {
        panic!("wrong receipt");
    };
    assert!(Arc::ptr_eq(notice, queued_notice));
    assert_eq!(notice.generation, 7);
    assert!(!queued.observed());
    notice.observe();
    assert!(queued.observed());
    assert_eq!(relations.wakes[&RunId::new(1)], (20, 9));

    let root = source(1, None);
    let mut unread = source(2, Some(1));
    unread.record.completion_generation = Some(37);
    let restored = PreparedRelations::prepare(&[root, unread]).unwrap();
    let notice = &restored.completions[&RunId::new(2)];
    assert_eq!(notice.generation, 37);
    assert!(!notice.observed());
    assert!(restored.queues.is_empty());
}

#[test]
fn consumed_receipt_has_no_queue_or_live_sender_dependency_and_duplicates_are_fenced() {
    let mut root = source(1, None);
    let mut consumed = delivery("msg-5");
    consumed.sender = RunId::new(99);
    consumed.state = DurableDeliveryState::Consumed;
    consumed.consumed_receipt = Some("msg-5".into());
    consumed.notice = DurableNotice::Explicit;
    consumed.message = None;
    root.record.deliveries = vec![consumed.clone(), consumed.clone()];
    let restored = PreparedRelations::prepare(&[root]).unwrap();
    assert!(restored.queues.is_empty());
    assert_eq!(restored.deliveries.len(), 1);

    let mut root = source(1, None);
    consumed.consumed_receipt = Some("wrong".into());
    root.record.deliveries.push(consumed);
    assert!(PreparedRelations::prepare(&[root]).is_err());

    let mut root = source(1, None);
    let original = delivery("msg-5");
    let mut conflicting = original.clone();
    conflicting.message.as_mut().unwrap().content = "different".into();
    root.record.deliveries = vec![original, conflicting];
    assert!(PreparedRelations::prepare(&[root, source(2, Some(1))]).is_err());
}

#[test]
fn question_messages_share_one_receipt_and_generation_conflicts_fail_closed() {
    let mut root = source(1, None);
    let mut first = delivery("msg-5");
    first.notice = DurableNotice::Question {
        id: "question-1".into(),
        parent: RunId::new(1),
        child: RunId::new(2),
        blocking: true,
        observed: false,
        admitted: true,
        paused: true,
    };
    let mut second = first.clone();
    second.message_id = "msg-6".into();
    second.message.as_mut().unwrap().message_id = second.message_id.clone();
    second.sequence = 1;
    root.record.deliveries = vec![first, second];
    let relations = PreparedRelations::prepare(&[root, source(2, Some(1))]).unwrap();
    let queue = &relations.queues[&RunId::new(1)];
    assert!(Arc::ptr_eq(
        queue[0].1.question_notice().unwrap(),
        queue[1].1.question_notice().unwrap()
    ));
    let notice = &relations.questions["question-1"];
    assert!(notice.paused.load(Ordering::Acquire));
    assert!(notice.admitted.load(Ordering::Acquire));
    assert!(!notice.observed());

    let mut root = source(1, None);
    root.record.deliveries.push(delivery("msg-5"));
    let mut child = source(2, Some(1));
    child.record.completion = Some(crate::quiesce::ObservedGeneration {
        id: "run-2".into(),
        generation: 8,
    });
    assert!(PreparedRelations::prepare(&[root, child]).is_err());
}
