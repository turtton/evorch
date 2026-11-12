use super::*;
use serde_json::json;

#[tokio::test]
async fn message_ids_advance_across_components_and_exhaust_without_wrapping() {
    struct Model;
    #[async_trait::async_trait]
    impl crate::AgentModel for Model {
        fn selected_model(&self, _: Role, _: Option<&str>) -> String {
            "fixed".into()
        }
        async fn complete(
            &self,
            _: &crate::AgentInvocationContext,
            _: Role,
            _: &[providers::Message],
            _: &[providers::ToolSpec],
        ) -> Result<providers::ChatResponse, RuntimeError> {
            std::future::pending().await
        }
    }
    let bus = Arc::new(event_bus::EventBus::new(64));
    let runtime = AgentRuntime::new(
        bus.clone(),
        Arc::new(tools::ToolExecutor::new(bus)),
        Arc::new(Model),
    );
    for (component, number) in [(1, 40), (2, 7), (3, 90)] {
        let mut root = source(component, None);
        root.record.root = root.record.run_id;
        root.record.next_message_id = number + 1;
        let mut d = delivery(&format!("msg-{number}"));
        d.recipient = root.record.run_id;
        d.state = DurableDeliveryState::Consumed;
        d.consumed_receipt = Some(d.message_id.clone());
        d.notice = DurableNotice::Explicit;
        d.message = None;
        root.record.deliveries.push(d);
        let mut relations = PreparedRelations::prepare(&[root]).unwrap();
        relations.publish(&runtime, &mut vec![]);
        let next = runtime.allocate_message_id().unwrap();
        assert_eq!(
            next,
            format!("msg-{}", if number == 7 { 42 } else { number + 1 })
        );
    }
    let mut root = source(1, None);
    let mut d = delivery(&format!("msg-{}", u64::MAX));
    d.state = DurableDeliveryState::Consumed;
    d.consumed_receipt = Some(d.message_id.clone());
    d.notice = DurableNotice::Explicit;
    d.message = None;
    root.record.deliveries.push(d);
    assert!(PreparedRelations::prepare(&[root]).is_err());
    runtime
        .shared
        .next_message_id
        .store(u64::MAX, Ordering::Release);
    assert!(runtime.allocate_message_id().is_err());
    assert!(runtime.allocate_message_id().is_err());
    assert_eq!(
        runtime.shared.next_message_id.load(Ordering::Acquire),
        u64::MAX
    );
}

#[test]
fn absent_or_understated_message_id_evidence_is_not_recoverable() {
    let mut root = source(1, None);
    let mut value = serde_json::to_value(&root.record).unwrap();
    value.as_object_mut().unwrap().remove("next_message_id");
    assert!(serde_json::from_value::<RunRecoveryRecord>(value).is_err());
    root.record.next_message_id = 0;
    assert!(PreparedRelations::prepare(std::slice::from_ref(&root)).is_err());
    root.record.next_message_id = 1;
    root.record.deliveries.push(delivery("msg-1"));
    let mut sources = [root, source(2, Some(1))];
    assert!(PreparedRelations::prepare(&sources).is_err());
    sources[0].record.next_message_id = 2;
    assert!(PreparedRelations::prepare(&sources).is_ok());
}

#[test]
fn component_ancestry_preserves_grandchild_metadata_before_publication() {
    let graph =
        PreparedRelations::prepare(&[source(1, None), source(2, Some(1)), source(3, Some(2))])
            .unwrap();
    assert!(!graph.ancestries[&RunId::new(1)].is_subagent);
    assert!(graph.ancestries[&RunId::new(2)].is_subagent);
    assert_eq!(graph.ancestries[&RunId::new(2)].parent_subagent_id, None);
    assert!(graph.ancestries[&RunId::new(3)].is_subagent);
    assert_eq!(
        graph.ancestries[&RunId::new(3)].parent_subagent_id,
        Some(RunId::new(2))
    );
}

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
            "user_reset_generation":9,"input_ids":[],"next_message_id":100
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
    first.message.as_mut().unwrap().sender_run_id = "runtime".into();
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
