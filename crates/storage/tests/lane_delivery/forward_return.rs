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
