//! Phase 2 storage contracts: deterministic integer clocks, explicit writer
//! completion and barriers. The shared fixture exercises the production writer.
use super::*;
#[path = "close.rs"]
mod close;
#[path = "forward_return.rs"]
mod forward_return;
#[path = "management.rs"]
mod management;
use storage::lane_control::*;
fn rejected<T: std::fmt::Debug>(r: Result<T, StorageError>, reason: LaneOperationError) {
    assert!(
        matches!(r,Err(StorageError::LaneOperation(actual)) if actual==reason),
        "expected {reason:?}, got {r:?}"
    );
}
/// The lead thread exists with its current root, which a lead close compares.
fn lead_thread(f: &Fixture) {
    f.h()
        .upsert_threads_host(&[storage::threads::ThreadUpsert {
            thread_id: "lead".into(),
            root_run_id: Some("lead-root".into()),
            chat_role: Some("Orchestrator".into()),
            ..Default::default()
        }])
        .unwrap();
}
fn started(f: &Fixture) -> LaneDeliveryOwner {
    lead_thread(f);
    f.lane(1, LanePolicy::Attached);
    f.h().start_lane_run("lane-1", 0, "run-1", 2).unwrap();
    let owner = LaneDeliveryOwner {
        root_run_id: "run-1".into(),
        generation: 1,
        ownership_generation: 7,
    };
    f.h()
        .bind_lane_delivery_destination(
            "thread-1".into(),
            0,
            owner.clone(),
            LaneDestinationAction::Bind,
        )
        .unwrap();
    owner
}
fn observation(f: &Fixture, owner: &LaneDeliveryOwner) -> ObserveLaneStall {
    ObserveLaneStall {
        lane_id: "lane-1".into(),
        management_revision: 1,
        owner: owner.clone(),
        progress_anchor: f.h().lane_progress_anchor("lane-1".into()).unwrap(),
        classification: LaneStallClassification::SilentDone,
        at_ns: 10,
    }
}
fn begin(f: &Fixture, n: u8) -> BeginLaneVerification {
    BeginLaneVerification {
        verification_id: VerificationId([n; 16]),
        lead_thread_id: "lead".into(),
        target: f.h().lane_verification_target("lane-1".into()).unwrap(),
        reviewer_run_id: format!("reviewer-{n}"),
        pr_evidence: None,
        caveats: vec![],
        at_ns: 10,
    }
}
fn submission() -> LaneVerificationSubmission {
    LaneVerificationSubmission {
        verdict: LaneVerificationVerdict::Approved,
        checks: vec![LaneCriterionCheck {
            criterion_index: 0,
            passed: true,
            evidence_refs: vec!["artifact:quality-check".into()],
        }],
        result_ref: "review-result".into(),
        result_digest: "c".repeat(64),
    }
}
fn done(f: &Fixture, run: &str, role: &str, phase: &str) {
    let mut record = snapshot().record;
    record.run_id = run.into();
    record.role = role.into();
    record.parent_run_id = Some("lead-root".into());
    record.terminal_phase = phase.into();
    record.updated_at_ns = 20;
    f.h().upsert_run_context(&record).unwrap();
}
fn lead() -> LaneCloser {
    LaneCloser::Lead {
        thread_id: "lead".into(),
        root_run_id: "lead-root".into(),
    }
}
/// The close a lead would send after observing `r.target`, with `r` attached for reference.
fn close_request(r: &BeginLaneVerification) -> CloseLane {
    CloseLane {
        closer: lead(),
        kind: LaneCloseKind::LeadClosed,
        lane_id: r.target.lane_id.clone(),
        ready_seq: Some(r.target.ready_seq),
        expected_management_revision: r.target.management_revision,
        expected_request_revision: r.target.request_revision,
        note: "accepted".into(),
        verification_id: Some(r.verification_id),
        at_ns: 30,
    }
}
/// A bare lead close of whatever the lane looks like now, with nothing attached.
fn close_now(f: &Fixture) -> CloseLane {
    let c = f.h().lane_close_context("lane-1".into()).unwrap();
    CloseLane {
        closer: lead(),
        kind: LaneCloseKind::LeadClosed,
        lane_id: "lane-1".into(),
        ready_seq: c.latest_ready_seq,
        expected_management_revision: c.management_revision,
        expected_request_revision: c.request_revision,
        note: "accepted on my own judgement".into(),
        verification_id: None,
        at_ns: 30,
    }
}
fn human_close(f: &Fixture, kind: LaneCloseKind) -> CloseLane {
    CloseLane {
        closer: LaneCloser::Human {
            surface: "web".into(),
            client_id: Some("client-1".into()),
        },
        kind,
        ready_seq: None,
        note: "ending this lane".into(),
        ..close_now(f)
    }
}
fn verified(f: &Fixture, n: u8) -> BeginLaneVerification {
    let r = begin(f, n);
    f.h().begin_lane_verification(r.clone()).unwrap();
    f.h()
        .submit_lane_verification(
            r.verification_id,
            r.reviewer_run_id.clone(),
            submission(),
            11,
        )
        .unwrap();
    done(f, &r.reviewer_run_id, "Reviewer", "Done");
    f.h()
        .complete_lane_verification(r.verification_id, r.reviewer_run_id.clone(), 21)
        .unwrap();
    r
}
fn child_question(f: &Fixture) -> ForwardLaneQuestion {
    let mut record = snapshot().record;
    record.run_id = "child".into();
    record.parent_run_id = Some("run-1".into());
    f.h().upsert_run_context(&record).unwrap();
    let mut q = question(1, 0);
    q.run_id = "child".into();
    q.root_run_id = "run-1".into();
    f.h().create_user_question(&q).unwrap();
    ForwardLaneQuestion {
        caller_root: "run-1".into(),
        question_id: q.id,
        expected_lane_generation: 1,
        expected_management_revision: 1,
        operation_id: "forward-op".into(),
        at_ns: 3,
    }
}
fn filter() -> LaneAttentionFilter {
    LaneAttentionFilter {
        lead_thread_id: "lead".into(),
        project_id: None,
    }
}

#[test]
fn stall_classification_cas_preserves_identity_and_slot_and_retires_unrecoverable_pending() {
    let f = Fixture::new();
    let owner = started(&f);
    let mut r = observation(&f, &owner);
    r.classification = LaneStallClassification::NoReport;
    let first = f.h().observe_lane_stall(r.clone()).unwrap();
    let pending = f
        .h()
        .request_lane_nudge(r.clone(), first.episode_id.clone(), first.revision)
        .unwrap();
    r.classification = LaneStallClassification::SilentDone;
    let done = f
        .h()
        .reclassify_lane_stall(r.clone(), first.episode_id.clone(), pending.revision)
        .unwrap();
    assert_eq!(done.episode_id, pending.episode_id);
    assert_eq!(done.notification_id, pending.notification_id);
    assert_eq!(done.nudge_state, LaneNudgeState::Pending);
    assert_eq!(done.revision, pending.revision + 1);
    rejected(
        f.h()
            .reclassify_lane_stall(r.clone(), first.episode_id.clone(), pending.revision),
        LaneOperationError::StaleEpisode,
    );
    assert_eq!(
        f.h()
            .request_lane_nudge(r.clone(), first.episode_id.clone(), done.revision)
            .unwrap()
            .notification_id,
        pending.notification_id
    );
    r.classification = LaneStallClassification::RestoreUnavailable;
    let failed = f
        .h()
        .reclassify_lane_stall(r.clone(), first.episode_id.clone(), done.revision)
        .unwrap();
    assert_eq!(failed.nudge_state, LaneNudgeState::Failed);
    let state: String = f
        .sql()
        .query_row(
            "SELECT state FROM lane_notifications WHERE notification_id=?1",
            [&pending.notification_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(state, "obsolete");
    assert_eq!(
        f.h()
            .request_lane_nudge(r, first.episode_id, failed.revision)
            .unwrap()
            .nudge_state,
        LaneNudgeState::Failed
    );
    assert_eq!(f.count("lane_stall_episodes"), 1);
    assert_eq!(f.count("lane_notifications"), 1);
}
#[test]
fn differently_worded_working_reports_and_generations_never_renew_nudge_slot() {
    let f = Fixture::new();
    let owner = started(&f);
    f.h()
        .append_lane_report("lane-1", 1, report(LaneStatus::Working), 3)
        .unwrap();
    let r = observation(&f, &owner);
    let e = f.h().observe_lane_stall(r.clone()).unwrap();
    let pending = f
        .h()
        .request_lane_nudge(r.clone(), e.episode_id.clone(), e.revision)
        .unwrap();
    assert_eq!(pending.nudge_state, LaneNudgeState::Pending);
    let f = f.reopen();
    for n in 0..3 {
        let mut report = report(LaneStatus::Working);
        report.summary = format!("different report text {n}");
        f.h()
            .append_lane_report("lane-1", 1, report, 12 + n)
            .unwrap();
        let mut observed = r.clone();
        observed.classification = LaneStallClassification::WorkStalled;
        let again = f.h().observe_lane_stall(observed.clone()).unwrap();
        assert_eq!(again.episode_id, e.episode_id);
        assert_eq!(again.resolved_at_ns, None);
        assert_eq!(
            f.h()
                .request_lane_nudge(observed, e.episode_id.clone(), e.revision)
                .unwrap()
                .notification_id,
            pending.notification_id
        );
    }
    f.h()
        .start_lane_run("lane-1", 1, "run-1-again", 30)
        .unwrap();
    assert_eq!(
        f.h().lane_progress_anchor("lane-1".into()).unwrap(),
        r.progress_anchor
    );
    assert_eq!(f.count("lane_stall_episodes"), 1);
    assert_eq!(f.count("lane_notifications"), 1);
}
#[test]
fn genuine_artifact_status_and_user_input_resolve_episodes_but_retry_does_not() {
    let f = Fixture::new();
    let owner = started(&f);
    for (status, sha) in [
        (LaneStatus::Working, 'a'),
        (LaneStatus::Working, 'b'),
        (LaneStatus::Ready, 'b'),
    ] {
        let r = observation(&f, &owner);
        let e = f.h().observe_lane_stall(r).unwrap();
        let mut report = report(status);
        report.artifact_revision = Some(ArtifactRevision::CommitSha(sha.to_string().repeat(40)));
        f.h().append_lane_report("lane-1", 1, report, 20).unwrap();
        assert!(
            f.h()
                .lane_stall_episode(e.episode_id)
                .unwrap()
                .unwrap()
                .resolved_at_ns
                .is_some()
        );
    }
    let r = observation(&f, &owner);
    let e = f.h().observe_lane_stall(r).unwrap();
    f.h()
        .accept_lane_user_input("lane-1".into(), user_receipt())
        .unwrap();
    assert!(
        f.h()
            .lane_stall_episode(e.episode_id)
            .unwrap()
            .unwrap()
            .resolved_at_ns
            .is_some()
    );
    let anchor = f.h().lane_progress_anchor("lane-1".into()).unwrap();
    f.h()
        .accept_lane_user_input("lane-1".into(), user_receipt())
        .unwrap();
    assert_eq!(f.h().lane_progress_anchor("lane-1".into()).unwrap(), anchor);
}
#[test]
fn nudge_done_wake_receipt_is_retryable_and_consumes_no_guard() {
    let f = Fixture::new();
    let owner = started(&f);
    let r = observation(&f, &owner);
    let e = f.h().observe_lane_stall(r.clone()).unwrap();
    f.h()
        .request_lane_nudge(r, e.episode_id.clone(), 1)
        .unwrap();
    let request = LaneClaimRequest {
        destination_thread_id: "thread-1".into(),
        expected_destination_epoch: 1,
        owner,
        token: "nudge-claim".into(),
        now_ns: 11,
        lease_until_ns: 100,
        allow_non_waking: false,
    };
    let LaneClaimResult::Claimed(claim) = f.h().claim_lane_deliveries(request).unwrap() else {
        panic!("Nudge must wake a Done root")
    };
    assert!(claim.completed_root_wake);
    let before = f.h().lane_wake_guard("lane-1".into()).unwrap();
    let anchor = f.h().lane_progress_anchor("lane-1".into()).unwrap();
    let mut write = snapshot();
    write.record.run_id = "run-1".into();
    f.h().upsert_run_context(&write.record).unwrap();
    let recovery = f.h().prepare_lane_recovery(claim.clone(), 11).unwrap();
    f.h()
        .accept_lane_delivery(claim.clone(), write.clone(), 12)
        .unwrap();
    assert_eq!(
        f.h()
            .accept_lane_delivery(claim.clone(), write, 13)
            .unwrap(),
        None
    );
    assert_eq!(f.h().lane_wake_guard("lane-1".into()).unwrap(), before);
    assert_eq!(f.h().lane_progress_anchor("lane-1".into()).unwrap(), anchor);
    assert_eq!(
        f.h()
            .lane_recovery_episode(recovery.episode.episode_id.clone())
            .unwrap()
            .unwrap(),
        recovery.episode,
        "Nudge acceptance is not ordinary input progress"
    );
    assert_eq!(
        f.h()
            .lane_stall_episode(e.episode_id)
            .unwrap()
            .unwrap()
            .nudge_state,
        LaneNudgeState::Accepted
    );
    f.h().ack_lane_delivery(claim.clone(), 14).unwrap();
    f.h().ack_lane_delivery(claim, 15).unwrap();
    f.h()
        .append_lane_report_with_delivery("lane-1".into(), 1, report(LaneStatus::Ready), 16)
        .unwrap();
    assert!(matches!(
        f.h().claim_lane_deliveries(super::request(17)).unwrap(),
        LaneClaimResult::Claimed(_)
    ));
}
#[test]
fn nudge_cas_is_single_slot_across_two_writers_and_rejects_stale_owner() {
    let f = Fixture::new();
    let owner = started(&f);
    let r = observation(&f, &owner);
    let e = f.h().observe_lane_stall(r.clone()).unwrap();
    let other = Storage::open(f.config.clone()).unwrap();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let a = barrier.clone();
    let h = other.handle();
    let req = r.clone();
    let id = e.episode_id.clone();
    let worker = std::thread::spawn(move || {
        a.wait();
        h.request_lane_nudge(req, id, 1).unwrap()
    });
    barrier.wait();
    let one = f
        .h()
        .request_lane_nudge(r.clone(), e.episode_id, 1)
        .unwrap();
    let two = worker.join().unwrap();
    assert_eq!(one.notification_id, two.notification_id);
    assert_eq!(f.count("lane_notifications"), 1);
    let mut stale = r;
    stale.owner.ownership_generation += 1;
    rejected(
        f.h().observe_lane_stall(stale),
        LaneOperationError::OwnershipLost,
    );
    other.close();
}
#[test]
fn forward_is_atomic_retryable_preserves_provenance_and_blocks_answer_bypasses() {
    let f = Fixture::new();
    started(&f);
    let r = child_question(&f);
    f.h()
        .bind_user_questions(
            "child",
            "child-continuation",
            std::slice::from_ref(&r.question_id),
        )
        .unwrap();
    let links = f.count("user_question_links");
    let route = f.h().forward_lane_question(r.clone()).unwrap();
    assert_eq!(f.h().forward_lane_question(r.clone()).unwrap(), route);
    assert_eq!(f.count("user_question_links"), links);
    assert_eq!(f.count("user_questions"), 1);
    let before: String = f
        .sql()
        .query_row(
            "SELECT payload FROM user_questions WHERE id=?1",
            [&r.question_id],
            |r| r.get(0),
        )
        .unwrap();
    for origin in [
        AnswerOrigin::Host {
            channel: Default::default(),
        },
        AnswerOrigin::ParentAgent {
            answered_by_run_id: "run-1".into(),
        },
    ] {
        rejected(
            f.h()
                .answer_user_question(&r.question_id, "bypass", &origin),
            LaneOperationError::NotResponder,
        );
        assert_eq!(
            f.sql()
                .query_row(
                    "SELECT payload FROM user_questions WHERE id=?1",
                    [&r.question_id],
                    |r| r.get::<_, String>(0)
                )
                .unwrap(),
            before
        );
        assert_eq!(f.count("lane_notifications"), 1);
        assert_eq!(f.count("lane_forward_receipts"), 1);
        assert_eq!(
            f.h()
                .lane_question_routing(r.question_id.clone())
                .unwrap()
                .unwrap(),
            route
        );
    }
    let mut other = r.clone();
    other.operation_id = "competing-op".into();
    rejected(
        f.h().forward_lane_question(other),
        LaneOperationError::StaleRouting,
    );
    f.h()
        .mutate_lane_question(LaneQuestionMutation {
            lane_id: "lane-1".into(),
            question_id: r.question_id.clone(),
            lead_thread_id: "lead".into(),
            expected_management_revision: 1,
            expected_routing_revision: 1,
            operation_id: "route-user".into(),
            action: LaneQuestionAction::RouteToUser,
            at_ns: 4,
        })
        .unwrap();
    f.h().forward_lane_question(r.clone()).unwrap(); // historical receipt, no route reversal
    assert_eq!(
        f.h()
            .lane_question_routing(r.question_id.clone())
            .unwrap()
            .unwrap()
            .responder,
        QuestionResponder::User
    );
    f.h()
        .answer_user_question(
            &r.question_id,
            "user answer",
            &AnswerOrigin::Host {
                channel: Default::default(),
            },
        )
        .unwrap();
    let q = f
        .db()
        .user_questions_for_run("child-continuation")
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(q.id, r.question_id);
    assert_eq!(q.run_id, "child");
    assert_eq!(q.root_run_id, "run-1");
    assert_eq!(q.answer.as_deref(), Some("user answer"));
    assert_eq!(f.count("user_question_links"), links);
}
#[test]
fn two_forward_operations_race_without_duplicating_route_outbox_or_receipt() {
    let f = Fixture::new();
    started(&f);
    let request = child_question(&f);
    let other = Storage::open(f.config.clone()).unwrap();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let a = barrier.clone();
    let h = other.handle();
    let mut concurrent = request.clone();
    concurrent.operation_id = "other-forward".into();
    let worker = std::thread::spawn(move || {
        a.wait();
        h.forward_lane_question(concurrent)
    });
    barrier.wait();
    let one = f.h().forward_lane_question(request);
    let two = worker.join().unwrap();
    assert_ne!(one.is_ok(), two.is_ok());
    rejected(
        if one.is_err() { one } else { two },
        LaneOperationError::StaleRouting,
    );
    assert_eq!(f.count("lane_question_routes"), 1);
    assert_eq!(f.count("lane_notifications"), 1);
    assert_eq!(f.count("lane_forward_receipts"), 1);
    other.close();
}
#[test]
fn forward_rollback_and_parent_answer_before_transfer_leave_no_routing() {
    let f = Fixture::new();
    started(&f);
    let r = child_question(&f);
    f.sql().execute_batch("CREATE TRIGGER fail_forward BEFORE INSERT ON lane_forward_receipts BEGIN SELECT RAISE(ABORT,'injected'); END;").unwrap();
    assert!(f.h().forward_lane_question(r.clone()).is_err());
    assert_eq!(f.count("lane_question_routes"), 0);
    assert_eq!(f.count("lane_notifications"), 0);
    f.sql().execute_batch("DROP TRIGGER fail_forward").unwrap();
    f.h()
        .answer_user_question(
            &r.question_id,
            "parent wins",
            &AnswerOrigin::ParentAgent {
                answered_by_run_id: "run-1".into(),
            },
        )
        .unwrap();
    rejected(
        f.h().forward_lane_question(r),
        LaneOperationError::AlreadyAnswered,
    );
    assert_eq!(f.count("lane_forward_receipts"), 0);
}
#[test]
fn forward_requires_actual_direct_child_not_an_inherited_question_link() {
    let f = Fixture::new();
    started(&f);
    let r = child_question(&f);
    f.sql()
        .execute(
            "UPDATE run_contexts SET parent_run_id='someone-else' WHERE run_id='child'",
            [],
        )
        .unwrap();
    f.h()
        .bind_user_questions("child", "run-1", std::slice::from_ref(&r.question_id))
        .unwrap();
    rejected(
        f.h().forward_lane_question(r),
        LaneOperationError::OwnershipLost,
    );
    assert_eq!(f.count("lane_question_routes"), 0);
}
#[test]
fn verification_submit_is_bound_to_reserved_run_and_is_one_shot() {
    let f = Fixture::new();
    started(&f);
    f.h()
        .append_lane_report("lane-1", 1, report(LaneStatus::Ready), 3)
        .unwrap();
    let r = begin(&f, 1);
    let v = f.h().begin_lane_verification(r.clone()).unwrap();
    assert_eq!(f.h().begin_lane_verification(r.clone()).unwrap(), v);
    for run in ["lead-root", "worker", "advisor", "reviewer-2"] {
        rejected(
            f.h()
                .submit_lane_verification(r.verification_id, run.into(), submission(), 11),
            LaneOperationError::NotResponder,
        );
    }
    f.h()
        .submit_lane_verification(
            r.verification_id,
            r.reviewer_run_id.clone(),
            submission(),
            11,
        )
        .unwrap();
    rejected(
        f.h().submit_lane_verification(
            r.verification_id,
            r.reviewer_run_id.clone(),
            submission(),
            12,
        ),
        LaneOperationError::AlreadySubmitted,
    );
    // A review that has only submitted is attachable and never blocks a close.
    let early = f.h().close_lane(close_request(&r)).unwrap();
    assert_eq!(
        early.review.as_ref().unwrap().state,
        LaneVerificationState::Submitted
    );
    let f = f.reopen();
    assert_eq!(
        f.h()
            .lane_verification(r.verification_id)
            .unwrap()
            .unwrap()
            .state,
        LaneVerificationState::Submitted
    );
    done(&f, &r.reviewer_run_id, "Reviewer", "Error");
    rejected(
        f.h()
            .complete_lane_verification(r.verification_id, r.reviewer_run_id.clone(), 21),
        LaneOperationError::ReviewerIncomplete,
    );
    done(&f, &r.reviewer_run_id, "Reviewer", "Done");
    f.h()
        .complete_lane_verification(r.verification_id, r.reviewer_run_id.clone(), 21)
        .unwrap();
    // The close decided earlier keeps the state it saw; later completion rewrites nothing.
    let again = f.h().close_lane(close_request(&r)).unwrap();
    assert_eq!(again.outcome, LaneCloseOutcome::AlreadyClosed);
    assert_eq!(again.close_id, early.close_id);
    assert_eq!(
        again.review.as_ref().unwrap().state,
        LaneVerificationState::Submitted
    );
    assert!(!again.review.unwrap().artifact_rechecked_at_close);
    assert_eq!(f.count("lane_closes"), 1);
}
#[test]
fn incomplete_and_rejected_reviews_are_recorded_but_never_block_a_close() {
    let f = Fixture::new();
    started(&f);
    f.h()
        .append_lane_report("lane-1", 1, report(LaneStatus::Ready), 3)
        .unwrap();
    let r = begin(&f, 1);
    f.h().begin_lane_verification(r.clone()).unwrap();
    done(&f, &r.reviewer_run_id, "Reviewer", "Done");
    rejected(
        f.h()
            .complete_lane_verification(r.verification_id, r.reviewer_run_id.clone(), 21),
        LaneOperationError::ReviewerIncomplete,
    );
    let mut missing = submission();
    missing.checks.clear();
    rejected(
        f.h()
            .submit_lane_verification(r.verification_id, r.reviewer_run_id.clone(), missing, 11),
        LaneOperationError::VerificationRejected,
    );
    let mut result = submission();
    result.verdict = LaneVerificationVerdict::Rejected;
    result.checks[0].passed = false;
    f.h()
        .submit_lane_verification(r.verification_id, r.reviewer_run_id.clone(), result, 11)
        .unwrap();
    f.h()
        .complete_lane_verification(r.verification_id, r.reviewer_run_id.clone(), 21)
        .unwrap();
    let closed = f.h().close_lane(close_request(&r)).unwrap();
    let review = closed.review.unwrap();
    assert_eq!(closed.kind, LaneCloseKind::LeadClosed);
    assert_eq!(review.state, LaneVerificationState::Completed);
    assert_eq!(review.verdict, Some(LaneVerificationVerdict::Rejected));
    assert!(review.targets_this_ready);
}
#[test]
fn target_changes_reject_a_stale_close_clear_the_current_one_and_keep_late_reviews() {
    // Reviews are references: a review that finishes after the lane moved on is kept.
    // A close decided on an older observation is rejected, never applied.
    for change in ["report", "brief", "input", "artifact"] {
        let f = Fixture::new();
        started(&f);
        f.h()
            .append_lane_report("lane-1", 1, report(LaneStatus::Ready), 3)
            .unwrap();
        let r = verified(&f, 1);
        let closed = f.h().close_lane(close_request(&r)).unwrap();
        let pending = begin(&f, 2);
        f.h().begin_lane_verification(pending.clone()).unwrap();
        match change {
            "brief" => {
                let mut brief = f.db().lane("lane-1").unwrap().unwrap().brief;
                brief.objective = "revised objective".into();
                f.h().revise_lane_brief("lane-1", 1, brief, 40).unwrap();
            }
            "input" => {
                f.h()
                    .accept_lane_user_input("lane-1".into(), user_receipt())
                    .unwrap();
            }
            _ => {
                let mut report = report(LaneStatus::Ready);
                if change == "artifact" {
                    report.artifact_revision = Some(ArtifactRevision::CommitSha("b".repeat(40)));
                }
                f.h().append_lane_report("lane-1", 1, report, 40).unwrap();
            }
        }
        f.h()
            .submit_lane_verification(
                pending.verification_id,
                pending.reviewer_run_id,
                submission(),
                41,
            )
            .unwrap();
        assert!(f.h().close_lane(close_request(&r)).is_err());
        assert_eq!(
            f.sql()
                .query_row(
                    "SELECT current_close_id FROM lanes WHERE lane_id='lane-1'",
                    [],
                    |row| row.get::<_, Option<String>>(0)
                )
                .unwrap(),
            None
        );
        let history: LaneCloseResult = serde_json::from_str(
            &f.sql()
                .query_row(
                    "SELECT payload FROM lane_closes WHERE close_id=?1",
                    [closed.close_id],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
        )
        .unwrap();
        assert_eq!(history.review.unwrap().verification_id, r.verification_id);
    }
}
#[test]
fn verification_and_close_fail_atomically_and_can_retry() {
    let f = Fixture::new();
    started(&f);
    f.h()
        .append_lane_report("lane-1", 1, report(LaneStatus::Ready), 3)
        .unwrap();
    let r = verified(&f, 1);
    f.sql().execute_batch("CREATE TRIGGER fail_close BEFORE INSERT ON lane_notifications WHEN NEW.event_kind='closed' BEGIN SELECT RAISE(ABORT,'injected'); END;").unwrap();
    assert!(f.h().close_lane(close_request(&r)).is_err());
    assert_eq!(f.count("lane_closes"), 0);
    assert_eq!(
        f.sql()
            .query_row(
                "SELECT current_close_id FROM lanes WHERE lane_id='lane-1'",
                [],
                |r| r.get::<_, Option<String>>(0)
            )
            .unwrap(),
        None
    );
    f.sql().execute_batch("DROP TRIGGER fail_close").unwrap();
    f.h().close_lane(close_request(&r)).unwrap();
    assert_eq!(f.count("lane_closes"), 1);
}
#[test]
fn attention_pages_are_bounded_stable_restartable_and_bound_to_filter_and_revision() {
    let f = Fixture::new();
    for n in 1..=25 {
        f.lane(n, LanePolicy::Attached);
        let mut report = report(LaneStatus::Blocked);
        report.summary = "\"😀".repeat(100);
        f.h()
            .append_lane_report(&format!("lane-{n}"), 1, report, i64::from(n))
            .unwrap();
    }
    let first = f.h().lane_attention(filter(), None, 100).unwrap();
    assert!(first.items.len() <= 20);
    assert!(serde_json::to_vec(&first).unwrap().len() <= 16 * 1024);
    assert!(first.next_cursor.is_some());
    let f = f.reopen();
    let mut ids = std::collections::HashSet::new();
    let mut page = first;
    loop {
        for item in page.items {
            assert!(ids.insert(item.id));
        }
        let Some(cursor) = page.next_cursor else {
            break;
        };
        page = f.h().lane_attention(filter(), Some(cursor), 20).unwrap();
        assert!(serde_json::to_vec(&page).unwrap().len() <= 16 * 1024);
    }
    assert_eq!(ids.len(), 25);
    let cursor = f
        .h()
        .lane_attention(filter(), None, 1)
        .unwrap()
        .next_cursor
        .unwrap();
    let mut different = filter();
    different.project_id = Some("other".into());
    rejected(
        f.h().lane_attention(different, Some(cursor.clone()), 1),
        LaneOperationError::StaleCursor,
    );
    f.h()
        .append_lane_report("lane-1", 1, report(LaneStatus::Working), 40)
        .unwrap();
    rejected(
        f.h().lane_attention(filter(), Some(cursor), 1),
        LaneOperationError::StaleCursor,
    );
}
#[test]
fn attention_excludes_detached_and_old_management_and_resolves_reports_and_questions() {
    let f = Fixture::new();
    started(&f);
    let request = child_question(&f);
    f.h().forward_lane_question(request.clone()).unwrap();
    f.h()
        .append_lane_report("lane-1", 1, report(LaneStatus::Blocked), 4)
        .unwrap();
    assert_eq!(
        f.h()
            .lane_attention(filter(), None, 20)
            .unwrap()
            .items
            .len(),
        2
    );
    f.h()
        .answer_user_question(
            &request.question_id,
            "answer",
            &AnswerOrigin::LaneLead {
                run_id: "lead-root".into(),
                thread_id: "lead".into(),
            },
        )
        .unwrap();
    f.h()
        .append_lane_report("lane-1", 1, report(LaneStatus::Working), 5)
        .unwrap();
    assert!(
        f.h()
            .lane_attention(filter(), None, 20)
            .unwrap()
            .items
            .is_empty()
    );
    f.h()
        .append_lane_report("lane-1", 1, report(LaneStatus::Ready), 6)
        .unwrap();
    let r = verified(&f, 1);
    f.h().close_lane(close_request(&r)).unwrap();
    assert!(
        f.h()
            .lane_attention(filter(), None, 20)
            .unwrap()
            .items
            .is_empty()
    );
    f.h()
        .apply_lane_management_transition(
            storage::lane_management::LaneManagementRequest {
                operation_id: format!("transition:{}", 40),
                lane_id: ("lane-1").to_string(),
                expected_management_revision: 1,
                action: storage::lane_management::LaneManagementAction::Detach,
            },
            storage::lane_management::LaneManagementActor::User,
            None,
            40,
        )
        .unwrap();
    assert!(
        f.h()
            .lane_attention(filter(), None, 20)
            .unwrap()
            .items
            .is_empty()
    );
    rejected(
        f.h().close_lane(close_request(&r)),
        LaneOperationError::NotAttached,
    );
}

#[test]
fn pending_nudge_is_obsoleted_by_progress_and_failed_or_suppressed_slot_never_renews() {
    for outcome in [LaneNudgeState::Failed, LaneNudgeState::Suppressed] {
        let f = Fixture::new();
        let owner = started(&f);
        let r = observation(&f, &owner);
        let e = f.h().observe_lane_stall(r.clone()).unwrap();
        let p = f
            .h()
            .request_lane_nudge(r.clone(), e.episode_id.clone(), 1)
            .unwrap();
        f.h()
            .record_lane_nudge_outcome(r.clone(), e.episode_id.clone(), p.revision, outcome)
            .unwrap();
        assert_eq!(
            f.h()
                .request_lane_nudge(r, e.episode_id.clone(), 1)
                .unwrap()
                .nudge_state,
            outcome
        );
        assert_eq!(f.count("lane_notifications"), 1);
    }
    let f = Fixture::new();
    let owner = started(&f);
    let r = observation(&f, &owner);
    let e = f.h().observe_lane_stall(r.clone()).unwrap();
    let pending = f
        .h()
        .request_lane_nudge(r.clone(), e.episode_id.clone(), 1)
        .unwrap();
    f.h()
        .accept_lane_user_input("lane-1".into(), user_receipt())
        .unwrap();
    assert_eq!(
        f.h()
            .lane_stall_episode(e.episode_id.clone())
            .unwrap()
            .unwrap()
            .nudge_state,
        LaneNudgeState::Obsolete
    );
    assert_eq!(
        f.sql()
            .query_row(
                "SELECT state FROM lane_notifications WHERE notification_id=?1",
                [pending.notification_id],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
        "obsolete"
    );
    rejected(
        f.h().request_lane_nudge(r, e.episode_id, 1),
        LaneOperationError::StaleEpisode,
    );
}
#[test]
fn nudge_enqueue_failure_rolls_back_slot_and_report_failure_rolls_back_revision() {
    let f = Fixture::new();
    let owner = started(&f);
    let r = observation(&f, &owner);
    let e = f.h().observe_lane_stall(r.clone()).unwrap();
    f.sql().execute_batch("CREATE TRIGGER abort_nudge BEFORE INSERT ON lane_notifications WHEN NEW.event_kind='nudge' BEGIN SELECT RAISE(ABORT,'injected'); END;").unwrap();
    assert!(
        f.h()
            .request_lane_nudge(r.clone(), e.episode_id.clone(), 1)
            .is_err()
    );
    assert_eq!(
        f.h()
            .lane_stall_episode(e.episode_id.clone())
            .unwrap()
            .unwrap(),
        e
    );
    f.sql().execute_batch("DROP TRIGGER abort_nudge; CREATE TRIGGER abort_report BEFORE UPDATE OF request_revision ON lanes BEGIN SELECT RAISE(ABORT,'injected'); END;").unwrap();
    assert!(
        f.h()
            .append_lane_report("lane-1", 1, report(LaneStatus::Ready), 4)
            .is_err()
    );
    assert_eq!(f.count("lane_reports"), 0);
    assert_eq!(
        f.h().lane_progress_anchor("lane-1".into()).unwrap(),
        r.progress_anchor
    );
    f.sql().execute_batch("DROP TRIGGER abort_report").unwrap();
    f.h().request_lane_nudge(r, e.episode_id, 1).unwrap();
    assert_eq!(f.count("lane_notifications"), 1);
}
#[test]
fn verification_submit_is_serialized_across_two_writers() {
    let f = Fixture::new();
    started(&f);
    f.h()
        .append_lane_report("lane-1", 1, report(LaneStatus::Ready), 3)
        .unwrap();
    let r = begin(&f, 1);
    f.h().begin_lane_verification(r.clone()).unwrap();
    let other = Storage::open(f.config.clone()).unwrap();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let a = barrier.clone();
    let h = other.handle();
    let req = r.clone();
    let worker = std::thread::spawn(move || {
        a.wait();
        h.submit_lane_verification(req.verification_id, req.reviewer_run_id, submission(), 11)
    });
    barrier.wait();
    let one =
        f.h()
            .submit_lane_verification(r.verification_id, r.reviewer_run_id, submission(), 11);
    let two = worker.join().unwrap();
    assert_ne!(one.is_ok(), two.is_ok());
    rejected(
        if one.is_err() { one } else { two },
        LaneOperationError::AlreadySubmitted,
    );
    other.close();
}
#[test]
fn lead_only_real_input_resolves_stall_without_invalidating_lane_verification() {
    let f = Fixture::new();
    let owner = started(&f);
    f.h()
        .append_lane_report("lane-1", 1, report(LaneStatus::Ready), 3)
        .unwrap();
    let r = begin(&f, 1);
    f.h().begin_lane_verification(r.clone()).unwrap();
    let e = f.h().observe_lane_stall(observation(&f, &owner)).unwrap();
    let mut record = snapshot().record;
    record.config_json = r#"{"thread_id":"lead"}"#.into();
    f.h().upsert_run_context(&record).unwrap();
    let mut receipt = user_receipt();
    receipt.run_id = "lead-root".into();
    receipt.root_run_id = "lead-root".into();
    f.h()
        .append_host_input_accepted(&[], &[], None, &receipt)
        .unwrap();
    assert!(
        f.h()
            .lane_stall_episode(e.episode_id)
            .unwrap()
            .unwrap()
            .resolved_at_ns
            .is_some()
    );
    assert_eq!(
        f.h().lane_verification_target("lane-1".into()).unwrap(),
        r.target
    );
    f.h()
        .submit_lane_verification(r.verification_id, r.reviewer_run_id, submission(), 11)
        .unwrap();
}
#[test]
fn canonical_lead_routing_stays_protected_if_the_lead_is_deleted() {
    let f = Fixture::new();
    started(&f);
    let r = child_question(&f);
    f.h().forward_lane_question(r.clone()).unwrap();
    f.sql().execute("INSERT INTO thread_tombstones(thread_id,family_key,deleted_at_ns) VALUES ('lead','family',11)",[]).unwrap();
    rejected(
        f.h().answer_user_question(
            &r.question_id,
            "host bypass",
            &AnswerOrigin::Host {
                channel: Default::default(),
            },
        ),
        LaneOperationError::NotResponder,
    );
    assert!(
        !f.sql()
            .query_row(
                "SELECT answered FROM user_questions WHERE id=?1",
                [r.question_id],
                |r| r.get::<_, bool>(0)
            )
            .unwrap()
    );
}
#[test]
fn attention_derives_delivery_failure_stop_capacity_guard_and_stall_and_resolves_them() {
    let f = Fixture::new();
    let owner = started(&f);
    let r = observation(&f, &owner);
    f.h().observe_lane_stall(r.clone()).unwrap();
    let mut failed = report(LaneStatus::Failed);
    failed.failure_episode = Some("failure-episode".into());
    f.h()
        .append_lane_report_with_delivery("lane-1".into(), 1, failed, 3)
        .unwrap();
    let c = claim(&f.h(), 10);
    f.h()
        .nack_lane_delivery(c, LaneDeliveryNack::RestoreFailed, 11)
        .unwrap();
    let page = f.h().lane_attention(filter(), None, 20).unwrap();
    assert!(
        page.items
            .iter()
            .any(|i| matches!(i.kind, LaneAttentionKind::Failed { .. }))
    );
    assert!(
        page.items
            .iter()
            .any(|i| matches!(i.kind, LaneAttentionKind::RestoreFailed { .. }))
    );
    f.h()
        .bind_lane_delivery_destination(
            "lead".into(),
            0,
            super::owner(),
            LaneDestinationAction::Stop,
        )
        .unwrap();
    for n in 0..32 {
        f.create(1, n, LaneDeliveryBinding::Unbound);
    }
    assert!(matches!(
        f.create(1, 33, LaneDeliveryBinding::Unbound),
        LaneQuestionCreated::CapacityBlocked(_)
    ));
    let mut guard = f.h().lane_wake_guard("lane-1".into()).unwrap();
    for n in 0..LANE_NO_PROGRESS_LIMIT {
        guard = f
            .h()
            .update_lane_wake_guard(
                "lane-1".into(),
                guard.revision,
                LaneGuardSignal::WakeBatch {
                    batch_id: format!("batch-{n}"),
                },
            )
            .unwrap();
    }
    let current = observation(&f, &owner);
    f.h().observe_lane_stall(current).unwrap();
    let page = f.h().lane_attention(filter(), None, 20).unwrap(); // questions use the first page, so scan all
    let mut all = page.items;
    let mut cursor = page.next_cursor;
    while let Some(token) = cursor {
        let page = f.h().lane_attention(filter(), Some(token), 20).unwrap();
        all.extend(page.items);
        cursor = page.next_cursor;
    }
    assert!(
        all.iter()
            .any(|i| matches!(i.kind, LaneAttentionKind::QuestionCapacityBlocked { .. }))
    );
    assert!(
        all.iter()
            .any(|i| matches!(i.kind, LaneAttentionKind::WakeSuppressed { .. }))
    );
    assert!(
        all.iter()
            .any(|i| matches!(i.kind, LaneAttentionKind::StopPending { .. }))
    );
    assert!(
        all.iter()
            .any(|i| matches!(i.kind, LaneAttentionKind::Stall { .. }))
    );
    f.h()
        .accept_lane_user_input("lane-1".into(), user_receipt())
        .unwrap();
    f.h()
        .bind_lane_delivery_destination(
            "lead".into(),
            1,
            super::owner(),
            LaneDestinationAction::ExplicitUserResume,
        )
        .unwrap();
    // Capacity is a projection of the current quota, not a sticky diagnostic.
    f.sql()
        .execute(
            "DELETE FROM user_questions WHERE id IN (SELECT id FROM user_questions LIMIT 1)",
            [],
        )
        .unwrap();
    f.h()
        .append_lane_report("lane-1", 1, report(LaneStatus::Working), 50)
        .unwrap();
    let mut cursor = None;
    let mut all = vec![];
    loop {
        let page = f.h().lane_attention(filter(), cursor, 20).unwrap();
        all.extend(page.items);
        cursor = page.next_cursor;
        if cursor.is_none() {
            break;
        }
    }
    assert!(!all.iter().any(|i| matches!(
        i.kind,
        LaneAttentionKind::QuestionCapacityBlocked { .. }
            | LaneAttentionKind::WakeSuppressed { .. }
            | LaneAttentionKind::StopPending { .. }
            | LaneAttentionKind::Stall { .. }
            | LaneAttentionKind::Failed { .. }
    )));
}
#[test]
fn pr_facts_are_bound_to_a_review_but_a_close_never_checks_or_fetches_them() {
    use storage::pull_requests::*;
    let f = Fixture::new();
    started(&f);
    let key = PullRequestKey::github("owner/repo", 7).unwrap();
    f.h()
        .link_pull_request(LinkPullRequest {
            thread_id: "thread-1".into(),
            key: key.clone(),
            expected_revision: 0,
            at_ns: 2,
        })
        .unwrap();
    rejected(
        f.h().lane_pr_evidence("thread-1".into()),
        LaneOperationError::EvidenceUnavailable,
    );
    let facts = PullRequestFacts {
        state: PullRequestState::Open,
        base_branch: "main".into(),
        head_repository: Some("owner/repo".into()),
        head_branch: "feature".into(),
        head_sha: "a".repeat(40),
        checks: vec![],
        checks_complete: true,
        review_decision: ReviewDecision::Approved,
    };
    let seq = f.h().begin_pull_request_fetch(&key).unwrap();
    f.h()
        .commit_pull_request_fetch(&key, seq, facts.clone(), 5)
        .unwrap();
    f.h()
        .record_pull_request_local_head("thread-1", 1, &"a".repeat(40), 6)
        .unwrap();
    f.h()
        .append_lane_report("lane-1", 1, report(LaneStatus::Ready), 7)
        .unwrap();
    let mut r = begin(&f, 1);
    r.pr_evidence = f.h().lane_pr_evidence("thread-1".into()).unwrap();
    f.h().begin_lane_verification(r.clone()).unwrap();
    f.h()
        .submit_lane_verification(
            r.verification_id,
            r.reviewer_run_id.clone(),
            submission(),
            11,
        )
        .unwrap();
    done(&f, &r.reviewer_run_id, "Reviewer", "Done");
    f.h()
        .complete_lane_verification(r.verification_id, r.reviewer_run_id.clone(), 21)
        .unwrap();
    let seq = f.h().begin_pull_request_fetch(&key).unwrap();
    f.h()
        .commit_pull_request_fetch(&key, seq, facts.clone(), 22)
        .unwrap();
    // The facts moved on after the review; the close records the review's snapshot.
    let closed = f.h().close_lane(close_request(&r)).unwrap();
    assert_eq!(closed.review.unwrap().pr_evidence, r.pr_evidence);
    assert_eq!(
        f.h()
            .pull_request_snapshot(&key)
            .unwrap()
            .unwrap()
            .fetch_seq,
        seq
    );
    let stored = f.h().lane_verification(r.verification_id).unwrap().unwrap();
    assert_eq!(stored.pr_evidence, r.pr_evidence); // original verification is immutable
    let failed = f.h().begin_pull_request_fetch(&key).unwrap();
    f.h()
        .fail_pull_request_fetch(
            &key,
            failed,
            FetchFailure {
                kind: FetchErrorKind::Network,
                message: "unavailable".into(),
            },
            24,
        )
        .unwrap();
    rejected(
        f.h().lane_pr_evidence("thread-1".into()),
        LaneOperationError::EvidenceUnavailable,
    );
    // A failed fetch or another PR changes nothing about the close that stands.
    assert_eq!(
        f.h().close_lane(close_request(&r)).unwrap().outcome,
        LaneCloseOutcome::AlreadyClosed
    );
    f.h()
        .link_pull_request(LinkPullRequest {
            thread_id: "thread-1".into(),
            key: PullRequestKey::github("owner/repo", 8).unwrap(),
            expected_revision: 1,
            at_ns: 25,
        })
        .unwrap();
    assert_eq!(
        f.h().close_lane(close_request(&r)).unwrap().outcome,
        LaneCloseOutcome::AlreadyClosed
    );
}
#[test]
fn late_writes_are_fenced_by_lane_and_reviewer_tombstones() {
    let f = Fixture::new();
    let owner = started(&f);
    let req = child_question(&f);
    f.h().forward_lane_question(req).unwrap();
    f.h()
        .append_lane_report("lane-1", 1, report(LaneStatus::Ready), 3)
        .unwrap();
    let r = verified(&f, 1);
    f.h().close_lane(close_request(&r)).unwrap();
    f.h().observe_lane_stall(observation(&f, &owner)).unwrap();
    f.sql().execute("INSERT INTO thread_tombstones(thread_id,family_key,deleted_at_ns) VALUES ('thread-1','family',50)",[]).unwrap();
    for table in ["lane_stall_episodes", "lane_verifications", "lane_closes"] {
        assert!(
            f.sql()
                .execute(&format!("UPDATE {table} SET payload=payload"), [])
                .is_err()
        );
    }
    assert!(
        f.sql()
            .execute(
                "UPDATE lane_forward_receipts SET result_json=result_json",
                []
            )
            .is_err()
    );
    assert!(
        f.h()
            .lane_attention(filter(), None, 20)
            .unwrap()
            .items
            .is_empty()
    );
    assert!(f.h().close_lane(close_request(&r)).is_err());
}

#[test]
fn lead_answer_acceptance_atomically_advances_target_and_progress_only_once() {
    let f = Fixture::new();
    let owner = started(&f);
    f.h()
        .append_lane_report("lane-1", 1, report(LaneStatus::Ready), 3)
        .unwrap();
    f.create(1, 0, LaneDeliveryBinding::Bound);
    let r = begin(&f, 1);
    f.h().begin_lane_verification(r.clone()).unwrap();
    let observed = observation(&f, &owner);
    let e = f.h().observe_lane_stall(observed.clone()).unwrap();
    f.h()
        .mutate_lane_question(LaneQuestionMutation {
            lane_id: "lane-1".into(),
            question_id: question(1, 0).id,
            lead_thread_id: "lead".into(),
            expected_management_revision: 1,
            expected_routing_revision: 1,
            operation_id: "answer".into(),
            action: LaneQuestionAction::Answer {
                text: "answer from lead".into(),
                origin: AnswerOrigin::LaneLead {
                    run_id: "lead-root".into(),
                    thread_id: "lead".into(),
                },
            },
            at_ns: 11,
        })
        .unwrap();
    assert_eq!(
        f.h().lane_verification_target("lane-1".into()).unwrap(),
        r.target
    );
    let request = LaneClaimRequest {
        destination_thread_id: "thread-1".into(),
        expected_destination_epoch: 1,
        owner,
        token: "answer-delivery".into(),
        now_ns: 12,
        lease_until_ns: 100,
        allow_non_waking: true,
    };
    let LaneClaimResult::Claimed(claim) = f.h().claim_lane_deliveries(request).unwrap() else {
        panic!("answer not queued")
    };
    let mut write = snapshot();
    write.record.run_id = "run-1".into();
    f.h()
        .accept_lane_delivery(claim.clone(), write.clone(), 13)
        .unwrap();
    let current = f.h().lane_verification_target("lane-1".into()).unwrap();
    assert_eq!(current.request_revision, r.target.request_revision + 1);
    assert_eq!(
        f.h().lane_progress_anchor("lane-1".into()).unwrap().epoch,
        observed.progress_anchor.epoch + 1
    );
    assert!(
        f.h()
            .lane_stall_episode(e.episode_id)
            .unwrap()
            .unwrap()
            .resolved_at_ns
            .is_some()
    );
    assert_eq!(f.h().accept_lane_delivery(claim, write, 14).unwrap(), None);
    assert_eq!(
        f.h().lane_verification_target("lane-1".into()).unwrap(),
        current
    );
    // The pending review is a reference for the older request: kept, not discarded.
    f.h()
        .submit_lane_verification(r.verification_id, r.reviewer_run_id, submission(), 15)
        .unwrap();
}

#[test]
fn mixed_nudge_batch_charges_only_the_ordinary_wake_source() {
    let f = Fixture::new();
    let owner = started(&f);
    let r = observation(&f, &owner);
    let e = f.h().observe_lane_stall(r.clone()).unwrap();
    f.h().request_lane_nudge(r, e.episode_id, 1).unwrap();
    let notice = LaneNotification {
        notification_id: "ordinary-milestone".into(),
        lane_id: "lane-1".into(),
        management_revision: 1,
        source_thread_id: "thread-1".into(),
        destination_thread_id: "thread-1".into(),
        kind: LaneNotificationKind::Milestone,
        report_seq: None,
        created_at_ns: 11,
    };
    f.sql().execute("INSERT INTO lane_notifications(notification_id,lane_id,management_revision,event_kind,semantic_key,source_thread_id,destination_thread_id,payload) VALUES (?1,'lane-1',1,'milestone','ordinary','thread-1','thread-1',?2)",rusqlite::params![notice.notification_id,serde_json::to_string(&notice).unwrap()]).unwrap();
    let request = LaneClaimRequest {
        destination_thread_id: "thread-1".into(),
        expected_destination_epoch: 1,
        owner,
        token: "mixed".into(),
        now_ns: 12,
        lease_until_ns: 100,
        allow_non_waking: false,
    };
    let LaneClaimResult::Claimed(claim) = f.h().claim_lane_deliveries(request).unwrap() else {
        panic!("mixed batch not claimed")
    };
    assert_eq!(claim.notifications.len(), 2);
    let mut write = snapshot();
    write.record.run_id = "run-1".into();
    f.h().accept_lane_delivery(claim, write, 13).unwrap();
    assert_eq!(
        f.h()
            .lane_wake_guard("lane-1".into())
            .unwrap()
            .no_progress_batches,
        1
    );
}

#[test]
fn first_report_without_artifact_resolves_no_report_only_for_terminal_status() {
    for status in [
        LaneStatus::Working,
        LaneStatus::Ready,
        LaneStatus::Blocked,
        LaneStatus::Waiting,
        LaneStatus::Failed,
    ] {
        let f = Fixture::new();
        let owner = started(&f);
        let mut r = observation(&f, &owner);
        r.classification = LaneStallClassification::NoReport;
        let episode = f.h().observe_lane_stall(r.clone()).unwrap();
        let pending = f
            .h()
            .request_lane_nudge(r.clone(), episode.episode_id.clone(), 1)
            .unwrap();
        let mut first = report(status);
        first.artifact_revision = None;
        if status == LaneStatus::Failed {
            first.failure_episode = Some("first-failure".into());
        }
        if status == LaneStatus::Ready {
            f.sql().execute_batch("CREATE TRIGGER fail_first_resolution BEFORE UPDATE ON lane_stall_episodes BEGIN SELECT RAISE(ABORT,'injected resolution failure'); END;").unwrap();
            assert!(
                f.h()
                    .append_lane_report("lane-1", 1, first.clone(), 11)
                    .is_err()
            );
            assert_eq!(f.count("lane_reports"), 0);
            assert_eq!(
                f.h().lane_progress_anchor("lane-1".into()).unwrap(),
                r.progress_anchor
            );
            assert_eq!(
                f.h()
                    .lane_stall_episode(episode.episode_id.clone())
                    .unwrap()
                    .unwrap(),
                pending
            );
            f.sql()
                .execute_batch("DROP TRIGGER fail_first_resolution")
                .unwrap();
        }
        f.h().append_lane_report("lane-1", 1, first, 12).unwrap();
        let resolved = matches!(status, LaneStatus::Ready | LaneStatus::Failed);
        let current = f
            .h()
            .lane_stall_episode(episode.episode_id)
            .unwrap()
            .unwrap();
        assert_eq!(current.resolved_at_ns.is_some(), resolved, "{status:?}");
        assert_eq!(
            current.nudge_state,
            if resolved {
                LaneNudgeState::Obsolete
            } else {
                LaneNudgeState::Pending
            }
        );
        let state: String = f
            .sql()
            .query_row(
                "SELECT state FROM lane_notifications WHERE notification_id=?1",
                [pending.notification_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(state, if resolved { "obsolete" } else { "pending" });
        assert_eq!(
            f.h().lane_progress_anchor("lane-1".into()).unwrap().epoch,
            r.progress_anchor.epoch + u64::from(resolved)
        );
        assert_eq!(
            f.h()
                .lane_attention(filter(), None, 20)
                .unwrap()
                .items
                .iter()
                .any(|item| matches!(item.kind, LaneAttentionKind::Stall { .. })),
            !resolved
        );
    }
}

#[test]
fn inherited_question_quota_attention_uses_the_same_recipient_set_as_creation() {
    let f = Fixture::new();
    started(&f);
    let mut ids = vec![];
    for n in 0..32 {
        let q = question(7, n);
        ids.push(q.id.clone());
        f.h().create_user_question(&q).unwrap();
    }
    f.h().bind_user_questions("run-7", "run-1", &ids).unwrap();
    let LaneQuestionCreated::CapacityBlocked(diagnostic) =
        f.create(1, 99, LaneDeliveryBinding::Unbound)
    else {
        panic!("inherited quota not enforced")
    };
    assert_eq!(diagnostic.requester_run_id, "run-1");
    assert_eq!(diagnostic.capacity, QuestionCapacity::Requester);
    let blocked = || {
        f.h()
            .lane_attention(filter(), None, 20)
            .unwrap()
            .items
            .iter()
            .any(|i| {
                matches!(
                    i.kind,
                    LaneAttentionKind::QuestionCapacityBlocked {
                        capacity: LaneQuestionCapacity::Requester,
                        limit: 32
                    }
                )
            })
    };
    assert!(blocked());
    // Answering frees an inherited slot: only unanswered questions count, for both
    // creation and attention.
    f.h()
        .answer_user_question(
            &ids[0],
            "resolved",
            &AnswerOrigin::Host {
                channel: Default::default(),
            },
        )
        .unwrap();
    assert!(!blocked());
    assert!(matches!(
        f.create(1, 100, LaneDeliveryBinding::Unbound),
        LaneQuestionCreated::Created(_)
    ));
    // The quota is full again (31 inherited + 1 own), so both sides report it again.
    assert!(blocked());
    assert!(matches!(
        f.create(1, 101, LaneDeliveryBinding::Unbound),
        LaneQuestionCreated::CapacityBlocked(_)
    ));
    // Retention/unlinking an unanswered recipient entry really removes it from the creation set.
    f.sql()
        .execute(
            "DELETE FROM user_question_links WHERE run_id='run-1' AND question_id=?1",
            [&ids[1]],
        )
        .unwrap();
    assert!(!blocked());
    assert!(matches!(
        f.create(1, 102, LaneDeliveryBinding::Unbound),
        LaneQuestionCreated::Created(_)
    ));
}

#[test]
fn working_waiting_reports_cannot_renew_the_nudge_slot() {
    let f = Fixture::new();
    let owner = started(&f);
    let mut input = report(LaneStatus::Working);
    input.artifact_revision = None;
    f.h()
        .append_lane_report_with_delivery("lane-1".into(), 1, input.clone(), 3)
        .unwrap();
    let initial = observation(&f, &owner);
    let episode = f.h().observe_lane_stall(initial.clone()).unwrap();
    let requested = f
        .h()
        .request_lane_nudge(
            initial.clone(),
            episode.episode_id.clone(),
            episode.revision,
        )
        .unwrap();
    for _ in 0..2 {
        for status in [LaneStatus::Waiting, LaneStatus::Working] {
            input.status = status;
            input.summary.push('.');
            input.question_refs = vec![format!("question-{}", input.summary.len())];
            f.h()
                .append_lane_report_with_delivery("lane-1".into(), 1, input.clone(), 4)
                .unwrap();
        }
    }
    let next = observation(&f, &owner);
    assert_eq!(
        next.progress_anchor, initial.progress_anchor,
        "state/sequence/question churn is not work progress"
    );
    let same = f.h().observe_lane_stall(next.clone()).unwrap();
    assert_eq!(same.episode_id, episode.episode_id);
    assert_eq!(
        f.h()
            .request_lane_nudge(next, same.episode_id, same.revision)
            .unwrap()
            .notification_id,
        requested.notification_id
    );
}
