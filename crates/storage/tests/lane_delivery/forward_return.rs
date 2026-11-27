use super::*;
fn user_route(f: &Fixture) -> String {
    started(f);
    let r = child_question(f);
    f.h().forward_lane_question(r.clone()).unwrap();
    f.h()
        .mutate_lane_question(LaneQuestionMutation {
            lane_id: "lane-1".into(),
            question_id: r.question_id.clone(),
            lead_thread_id: "lead".into(),
            expected_management_revision: 1,
            expected_routing_revision: 1,
            operation_id: "to-user".into(),
            action: LaneQuestionAction::RouteToUser,
            at_ns: 20,
        })
        .unwrap();
    r.question_id
}
fn host() -> AnswerOrigin {
    AnswerOrigin::Host {
        channel: Default::default(),
    }
}
#[test]
fn rerouted_host_answer_advances_projection_and_enqueues_exactly_one_return() {
    let f = Fixture::new();
    let id = user_route(&f);
    let before = f.db().user_question(&id).unwrap().unwrap();
    f.h().answer_user_question(&id, "human", &host()).unwrap();
    let q = f.db().user_question(&id).unwrap().unwrap();
    assert_eq!(q.id, before.id);
    assert_eq!(q.run_id, before.run_id);
    assert_eq!(q.root_run_id, before.root_run_id);
    assert_eq!(q.recipient_run_ids, before.recipient_run_ids);
    assert_eq!(q.routing_revision, before.routing_revision + 1);
    assert_eq!(q.answer_origin, Some(host()));
    let route = f.h().lane_question_routing(id.clone()).unwrap().unwrap();
    assert_eq!(route.routing_revision, q.routing_revision);
    assert_eq!(route.responder, QuestionResponder::User);
    let returned = f.h().lane_inbox("thread-1".into(), String::new()).unwrap();
    assert_eq!(returned.len(), 1);
    assert!(
        matches!(&returned[0].kind, LaneNotificationKind::Answer { question_id, routing_revision } if question_id == &id && *routing_revision == q.routing_revision)
    );
    f.h().answer_user_question(&id, "human", &host()).unwrap();
    assert_eq!(f.db().user_question(&id).unwrap().unwrap(), q);
    assert_eq!(
        f.h().lane_inbox("thread-1".into(), String::new()).unwrap(),
        returned
    );
}
#[test]
fn failed_host_return_enqueue_rolls_back_answer_and_routing() {
    let f = Fixture::new();
    let id = user_route(&f);
    let q = f.db().user_question(&id).unwrap().unwrap();
    let route = f.h().lane_question_routing(id.clone()).unwrap().unwrap();
    f.sql().execute_batch("CREATE TRIGGER reject_answer_return BEFORE INSERT ON lane_notifications WHEN NEW.event_kind='answer' BEGIN SELECT RAISE(ABORT,'injected return failure'); END;").unwrap();
    assert!(f.h().answer_user_question(&id, "human", &host()).is_err());
    assert_eq!(f.db().user_question(&id).unwrap().unwrap(), q);
    assert_eq!(
        f.h().lane_question_routing(id.clone()).unwrap().unwrap(),
        route
    );
    assert!(
        f.h()
            .lane_inbox("thread-1".into(), String::new())
            .unwrap()
            .is_empty()
    );
    f.sql()
        .execute_batch("DROP TRIGGER reject_answer_return;")
        .unwrap();
    f.h().answer_user_question(&id, "human", &host()).unwrap();
    assert_eq!(
        f.h()
            .lane_inbox("thread-1".into(), String::new())
            .unwrap()
            .len(),
        1
    );
}
#[test]
fn ordinary_host_answer_does_not_create_lane_routes_or_notifications() {
    let f = Fixture::new();
    let r = child_question(&f);
    f.h()
        .answer_user_question(&r.question_id, "ordinary", &host())
        .unwrap();
    let q = f.db().user_question(&r.question_id).unwrap().unwrap();
    assert_eq!(q.answer.as_deref(), Some("ordinary"));
    assert_eq!(q.routing_revision, 0);
    assert_eq!(f.count("lane_question_routes"), 0);
    assert_eq!(f.count("lane_notifications"), 0);
}

#[test]
fn failed_evidence_insert_rolls_back_host_answer_route_and_return_together() {
    let f = Fixture::new();
    let id = user_route(&f);
    let before = f.db().user_question(&id).unwrap().unwrap();
    let evidence = storage::ReviewEvidenceRow {
        id: "answer-evidence".into(),
        root_run_id: "run-1".into(),
        target_run_id: "child".into(),
        kind: "user_answer".into(),
        text: "human".into(),
        in_reply_to_question_id: Some(id.clone()),
        in_reply_to_title: Some(before.title.clone()),
        reference_kind: None,
        reference_context: None,
        attachments: Vec::new(),
        created_at_ns: 30,
    };
    f.sql().execute_batch("CREATE TRIGGER reject_answer_evidence BEFORE INSERT ON review_evidence BEGIN SELECT RAISE(ABORT,'injected evidence failure'); END;").unwrap();
    assert!(
        f.h()
            .answer_user_question_with_evidence(
                &id,
                "human",
                &host(),
                std::slice::from_ref(&evidence)
            )
            .is_err()
    );
    assert_eq!(f.db().user_question(&id).unwrap().unwrap(), before);
    assert_eq!(
        f.h()
            .lane_question_routing(id.clone())
            .unwrap()
            .unwrap()
            .routing_revision,
        before.routing_revision
    );
    assert!(
        f.h()
            .lane_inbox("thread-1".into(), String::new())
            .unwrap()
            .is_empty()
    );
    assert_eq!(f.count("review_evidence"), 0);
    f.sql()
        .execute_batch("DROP TRIGGER reject_answer_evidence;")
        .unwrap();
    f.h()
        .answer_user_question_with_evidence(&id, "human", &host(), &[evidence])
        .unwrap();
    assert_eq!(f.count("review_evidence"), 1);
    assert_eq!(
        f.h()
            .lane_inbox("thread-1".into(), String::new())
            .unwrap()
            .len(),
        1
    );
}

fn lead_notices(f: &Fixture) -> Vec<LaneNotification> {
    f.h().lane_inbox("lead".into(), String::new()).unwrap()
}
fn user_answered_for(id: &str, revision: u64) -> impl Fn(&LaneNotification) -> bool + '_ {
    move |n| {
        matches!(&n.kind, LaneNotificationKind::UserAnswered { question_id, routing_revision }
            if question_id == id && *routing_revision == revision)
    }
}

#[test]
fn user_answer_of_rerouted_question_gives_lead_one_bodyless_reference_that_survives_reopen() {
    let f = Fixture::new();
    let id = user_route(&f);
    assert!(lead_notices(&f).is_empty());
    let before = f.count("lane_notifications");
    f.h()
        .answer_user_question(&id, "secret human words", &host())
        .unwrap();
    let revision = f
        .h()
        .lane_question_routing(id.clone())
        .unwrap()
        .unwrap()
        .routing_revision;
    let lead = lead_notices(&f);
    assert_eq!(lead.len(), 1);
    assert!(user_answered_for(&id, revision)(&lead[0]));
    assert_eq!(lead[0].lane_id, "lane-1");
    assert_eq!(lead[0].source_thread_id, "thread-1");
    assert_eq!(lead[0].destination_thread_id, "lead");
    assert!(!serde_json::to_string(&lead[0]).unwrap().contains("secret"));
    // The lane alone received the answer; the lead gained neither a recipient
    // link nor an answer of its own.
    let lane = f.h().lane_inbox("thread-1".into(), String::new()).unwrap();
    assert_eq!(lane.len(), 1);
    assert!(matches!(lane[0].kind, LaneNotificationKind::Answer { .. }));
    assert!(
        f.db()
            .user_questions_for_run("lead-root")
            .unwrap()
            .is_empty()
    );
    assert!(
        f.h()
            .lane_questions("lead".into(), None)
            .unwrap()
            .questions
            .is_empty()
    );
    // A repeated answer, a repeated delivery and a restart keep one logical notice.
    f.h()
        .answer_user_question(&id, "secret human words", &host())
        .unwrap();
    let f = f.reopen();
    assert_eq!(lead_notices(&f), lead);
    assert_eq!(f.count("lane_notifications"), before + 2);
}

#[test]
fn lead_reference_is_non_waking_and_delivered_once_after_receipt() {
    let f = Fixture::new();
    let id = user_route(&f);
    f.h().answer_user_question(&id, "human", &host()).unwrap();
    let mut passive = request(10);
    passive.allow_non_waking = false;
    assert_eq!(
        f.h().claim_lane_deliveries(passive).unwrap(),
        LaneClaimResult::Empty
    );
    let first = claim(&f.h(), 11);
    assert_eq!(first.notifications.len(), 1);
    assert!(matches!(
        first.notifications[0].kind,
        LaneNotificationKind::UserAnswered { .. }
    ));
    f.h()
        .accept_lane_delivery(first.clone(), snapshot(), 12)
        .unwrap();
    f.h().ack_lane_delivery(first.clone(), 13).unwrap();
    f.h().ack_lane_delivery(first, 14).unwrap();
    let mut again = request(20);
    again.expected_destination_epoch = 0;
    assert_eq!(
        f.h().claim_lane_deliveries(again).unwrap(),
        LaneClaimResult::Empty
    );
}

#[test]
fn lead_reference_respects_stop() {
    let f = Fixture::new();
    let id = user_route(&f);
    f.h().answer_user_question(&id, "human", &host()).unwrap();
    f.h()
        .bind_lane_delivery_destination("lead".into(), 0, owner(), LaneDestinationAction::Stop)
        .unwrap();
    let mut stopped = request(10);
    stopped.expected_destination_epoch = 1;
    assert_eq!(
        f.h().claim_lane_deliveries(stopped).unwrap(),
        LaneClaimResult::Stopped
    );
}

#[test]
fn failed_lead_reference_enqueue_rolls_back_answer_and_lane_return() {
    let f = Fixture::new();
    let id = user_route(&f);
    let q = f.db().user_question(&id).unwrap().unwrap();
    let route = f.h().lane_question_routing(id.clone()).unwrap().unwrap();
    let before = f.count("lane_notifications");
    f.sql().execute_batch("CREATE TRIGGER reject_lead_reference BEFORE INSERT ON lane_notifications WHEN NEW.event_kind='user_answered' BEGIN SELECT RAISE(ABORT,'injected lead reference failure'); END;").unwrap();
    assert!(f.h().answer_user_question(&id, "human", &host()).is_err());
    assert_eq!(f.db().user_question(&id).unwrap().unwrap(), q);
    assert_eq!(
        f.h().lane_question_routing(id.clone()).unwrap().unwrap(),
        route
    );
    assert_eq!(f.count("lane_notifications"), before);
    f.sql()
        .execute_batch("DROP TRIGGER reject_lead_reference;")
        .unwrap();
    f.h().answer_user_question(&id, "human", &host()).unwrap();
    assert_eq!(f.count("lane_notifications"), before + 2);
}

#[test]
fn no_lead_reference_for_lead_answer_policy_fallback_or_detached_lane() {
    // The lead answers itself: the lane gets the Answer, no one else is told.
    let f = Fixture::new();
    started(&f);
    let r = child_question(&f);
    f.h().forward_lane_question(r.clone()).unwrap();
    f.h()
        .mutate_lane_question(LaneQuestionMutation {
            lane_id: "lane-1".into(),
            question_id: r.question_id.clone(),
            lead_thread_id: "lead".into(),
            expected_management_revision: 1,
            expected_routing_revision: 1,
            operation_id: "lead-answer".into(),
            action: LaneQuestionAction::Answer {
                text: "lead says".into(),
                origin: AnswerOrigin::LaneLead {
                    run_id: "lead-root".into(),
                    thread_id: "lead".into(),
                },
            },
            at_ns: 20,
        })
        .unwrap();
    assert!(
        lead_notices(&f)
            .iter()
            .all(|n| !matches!(n.kind, LaneNotificationKind::UserAnswered { .. }))
    );

    // User-routed only because delivery was never bound: not the lead's hand-over.
    let f = Fixture::new();
    f.lane(1, LanePolicy::Attached);
    let LaneQuestionCreated::Created(q) = f.create(1, 1, LaneDeliveryBinding::Unbound) else {
        panic!()
    };
    f.h().answer_user_question(&q.id, "human", &host()).unwrap();
    assert_eq!(f.count("lane_notifications"), 1);
    assert!(
        lead_notices(&f).is_empty(),
        "an unbound fallback is not a lead hand-over"
    );

    // Routed to the user, then detached before the answer: only the lane is told.
    let f = Fixture::new();
    let id = user_route(&f);
    f.h()
        .apply_lane_management_transition(
            storage::lane_management::LaneManagementRequest {
                operation_id: "detach".into(),
                lane_id: "lane-1".into(),
                expected_management_revision: 1,
                action: storage::lane_management::LaneManagementAction::Detach,
            },
            storage::lane_management::LaneManagementActor::User,
            None,
            40,
        )
        .unwrap();
    f.h().answer_user_question(&id, "human", &host()).unwrap();
    assert!(lead_notices(&f).is_empty());
    let lane = f.h().lane_inbox("thread-1".into(), String::new()).unwrap();
    assert_eq!(lane.len(), 1);
    assert!(matches!(lane[0].kind, LaneNotificationKind::Answer { .. }));
}
