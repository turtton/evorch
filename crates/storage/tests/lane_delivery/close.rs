//! ADR 0044: a close is the lead's (or a human's) management judgement. These cover what
//! it requires (current lead root, revisions, latest Ready for the current brief), what it
//! never requires (artifact revision, review, PR facts) and what it records.
use super::*;

fn ready_without_artifact(f: &Fixture) {
    let mut ready = report(LaneStatus::Ready);
    ready.artifact_revision = None;
    f.h().append_lane_report("lane-1", 1, ready, 3).unwrap();
}
fn payload(f: &Fixture, close_id: &str) -> LaneCloseResult {
    serde_json::from_str(
        &f.sql()
            .query_row(
                "SELECT payload FROM lane_closes WHERE close_id=?1",
                [close_id],
                |row| row.get::<_, String>(0),
            )
            .unwrap(),
    )
    .unwrap()
}
fn closed_notices(f: &Fixture) -> u64 {
    f.sql()
        .query_row(
            "SELECT COUNT(*) FROM lane_notifications WHERE event_kind='closed'",
            [],
            |r| r.get(0),
        )
        .unwrap()
}
fn second_ready() -> LaneReportInput {
    let mut ready = report(LaneStatus::Ready);
    ready.summary = "second ready".into();
    ready
}
fn states(f: &Fixture) -> (LaneReadyState, Vec<LaneAttentionAction>) {
    let item = f
        .h()
        .lane_attention(filter(), None, 20)
        .unwrap()
        .items
        .into_iter()
        .find(|i| matches!(i.kind, LaneAttentionKind::ReadyUnclosed { .. }))
        .unwrap();
    let LaneAttentionKind::ReadyUnclosed { readiness, .. } = item.kind else {
        unreachable!()
    };
    (readiness, item.available_actions)
}
fn revise_brief(f: &Fixture, at: i64) {
    let mut brief = f.db().lane("lane-1").unwrap().unwrap().brief;
    brief.objective = format!("revised objective {at}");
    f.h().revise_lane_brief("lane-1", 1, brief, at).unwrap();
}

#[test]
fn a_ready_without_any_artifact_or_review_closes_on_the_leads_judgement_alone() {
    let f = Fixture::new();
    started(&f);
    ready_without_artifact(&f);
    let closed = f.h().close_lane(close_now(&f)).unwrap();
    assert_eq!(closed.kind, LaneCloseKind::LeadClosed);
    assert_eq!(closed.outcome, LaneCloseOutcome::Closed);
    assert_eq!(closed.artifact_revision, None);
    assert_eq!(closed.review, None);
    assert_eq!(closed.note, "accepted on my own judgement");
    assert_eq!(closed.ready_seq, Some(1));
    // The deciding root is recorded from the trusted closer, immutably.
    assert_eq!(
        closed.closed_by,
        Some(LaneCloseActor::Lead {
            thread_id: "lead".into(),
            root_run_id: "lead-root".into()
        })
    );
    assert_eq!(
        closed.pr_context.as_ref().unwrap().status,
        LanePrContextStatus::NotLinked
    );
    assert_eq!(payload(&f, &closed.close_id), {
        let mut stored = closed.clone();
        stored.outcome = LaneCloseOutcome::Closed;
        stored
    });
    assert_eq!(f.count("lane_closes"), 1);
    assert_eq!(closed_notices(&f), 1);
}

#[test]
fn the_record_keeps_the_ready_as_reported_and_the_review_as_it_was_decided() {
    let f = Fixture::new();
    started(&f);
    f.h()
        .append_lane_report("lane-1", 1, report(LaneStatus::Ready), 3)
        .unwrap();
    let r = begin(&f, 1);
    let mut caveats = r.clone();
    caveats.caveats =
        vec!["artifact differs from the Ready's revision when the review started".into()];
    f.h().begin_lane_verification(caveats).unwrap();
    let closed = f.h().close_lane(close_request(&r)).unwrap();
    let review = closed.review.as_ref().unwrap();
    assert_eq!(review.state, LaneVerificationState::Pending);
    assert_eq!(review.verdict, None);
    assert!(review.targets_this_ready);
    assert!(!review.artifact_rechecked_at_close);
    assert_eq!(review.caveats.len(), 1);
    assert_eq!(
        closed.artifact_revision,
        Some(ArtifactRevision::CommitSha("a".repeat(40)))
    );
    assert_eq!(closed.report_brief_revision, Some(closed.brief_revision));
}

#[test]
fn a_review_of_another_lane_or_an_unknown_one_is_an_input_error() {
    let f = Fixture::new();
    started(&f);
    ready_without_artifact(&f);
    let mut unknown = close_now(&f);
    unknown.verification_id = Some(VerificationId([9; 16]));
    rejected(
        f.h().close_lane(unknown),
        LaneOperationError::VerificationMissing,
    );
    f.lane(2, LanePolicy::Attached);
    f.h().start_lane_run("lane-2", 0, "run-2", 2).unwrap();
    f.h()
        .append_lane_report("lane-2", 1, report(LaneStatus::Ready), 3)
        .unwrap();
    let mut other = begin(&f, 5);
    other.target = f.h().lane_verification_target("lane-2".into()).unwrap();
    f.h().begin_lane_verification(other.clone()).unwrap();
    let mut attach = close_now(&f);
    attach.verification_id = Some(other.verification_id);
    rejected(f.h().close_lane(attach), LaneOperationError::InvalidInput);
    assert_eq!(f.count("lane_closes"), 0);
}

#[test]
fn a_ready_for_an_older_brief_is_rejected_as_stale_brief() {
    let f = Fixture::new();
    started(&f);
    f.h()
        .append_lane_report("lane-1", 1, report(LaneStatus::Ready), 3)
        .unwrap();
    revise_brief(&f, 40);
    // The lead looked again after the revision, so every revision matches; only the
    // Ready itself is for the old brief.
    let now = f.h().lane_close_context("lane-1".into()).unwrap();
    assert_eq!(now.latest_ready_seq, Some(1));
    assert!(now.ready_brief_revision < Some(now.brief_revision));
    rejected(
        f.h().close_lane(close_now(&f)),
        LaneOperationError::StaleBrief,
    );
    assert_eq!(f.count("lane_closes"), 0);
    // A human may still end it, and a Ready for the new brief closes normally.
    f.h()
        .append_lane_report("lane-1", 1, second_ready(), 41)
        .unwrap();
    assert_eq!(f.h().close_lane(close_now(&f)).unwrap().ready_seq, Some(2));
}

#[test]
fn only_the_latest_ready_can_be_closed() {
    let f = Fixture::new();
    started(&f);
    f.h()
        .append_lane_report("lane-1", 1, report(LaneStatus::Ready), 3)
        .unwrap();
    let old = close_now(&f);
    f.h()
        .append_lane_report("lane-1", 1, report(LaneStatus::Working), 4)
        .unwrap();
    rejected(
        f.h().close_lane(old.clone()),
        LaneOperationError::StaleReady,
    );
    // Working is not a Ready, even for the current revisions.
    let mut working = close_now(&f);
    working.ready_seq = Some(2);
    rejected(f.h().close_lane(working), LaneOperationError::StaleReady);
    f.h()
        .append_lane_report("lane-1", 1, second_ready(), 5)
        .unwrap();
    rejected(f.h().close_lane(old), LaneOperationError::StaleReady);
    f.h().close_lane(close_now(&f)).unwrap();
}

#[test]
fn the_callers_root_is_compared_with_the_lead_thread_inside_the_close_transaction() {
    let f = Fixture::new();
    started(&f);
    ready_without_artifact(&f);
    let before_swap = close_now(&f);
    // A Stop followed by a restart replaced the root of the lead thread after the
    // runtime had authorized the caller and before the close committed.
    f.sql()
        .execute(
            "UPDATE threads SET root_run_id='lead-root-restarted' WHERE thread_id='lead'",
            [],
        )
        .unwrap();
    rejected(
        f.h().close_lane(before_swap),
        LaneOperationError::NotCurrentLead,
    );
    assert_eq!(f.count("lane_closes"), 0);
    assert_eq!(closed_notices(&f), 0);
    let mut restarted = close_now(&f);
    restarted.closer = LaneCloser::Lead {
        thread_id: "lead".into(),
        root_run_id: "lead-root-restarted".into(),
    };
    let closed = f.h().close_lane(restarted).unwrap();
    assert_eq!(
        closed.closed_by,
        Some(LaneCloseActor::Lead {
            thread_id: "lead".into(),
            root_run_id: "lead-root-restarted".into()
        })
    );
    // Not the lead thread at all.
    let f = Fixture::new();
    started(&f);
    ready_without_artifact(&f);
    let mut other_thread = close_now(&f);
    other_thread.closer = LaneCloser::Lead {
        thread_id: "somebody-else".into(),
        root_run_id: "lead-root".into(),
    };
    rejected(
        f.h().close_lane(other_thread),
        LaneOperationError::NotCurrentLead,
    );
}

#[test]
fn a_retry_returns_the_standing_close_and_never_reopens_a_cleared_one() {
    let f = Fixture::new();
    started(&f);
    ready_without_artifact(&f);
    let request = close_now(&f);
    let first = f.h().close_lane(request.clone()).unwrap();
    // The response was lost: the same request returns the same record, with the first
    // closer's note, and adds no row or notice.
    let mut retry = request.clone();
    retry.note = "a different wording".into();
    let again = f.h().close_lane(retry).unwrap();
    assert_eq!(again.outcome, LaneCloseOutcome::AlreadyClosed);
    assert_eq!(again.close_id, first.close_id);
    assert_eq!(again.note, first.note);
    assert_eq!(f.count("lane_closes"), 1);
    assert_eq!(closed_notices(&f), 1);
    // New input for the lane clears the close; the old request is stale, not a re-close.
    f.h()
        .accept_lane_user_input("lane-1".into(), user_receipt())
        .unwrap();
    assert_eq!(f.h().current_lane_close("lane-1".into()).unwrap(), None);
    rejected(f.h().close_lane(request), LaneOperationError::StaleRequest);
    assert_eq!(f.h().current_lane_close("lane-1".into()).unwrap(), None);
    assert_eq!(f.count("lane_closes"), 1);
}

#[test]
fn stale_management_and_request_revisions_are_rejected() {
    let f = Fixture::new();
    started(&f);
    ready_without_artifact(&f);
    let mut stale_management = close_now(&f);
    stale_management.expected_management_revision += 1;
    rejected(
        f.h().close_lane(stale_management),
        LaneOperationError::StaleManagement,
    );
    let mut stale_request = close_now(&f);
    stale_request.expected_request_revision += 1;
    rejected(
        f.h().close_lane(stale_request),
        LaneOperationError::StaleRequest,
    );
    assert_eq!(f.count("lane_closes"), 0);
}

#[test]
fn the_largest_note_references_review_and_caveats_still_fit_the_close_record() {
    let f = Fixture::new();
    started(&f);
    let mut ready = report(LaneStatus::Ready);
    ready.artifact_refs = (0..LANE_ARTIFACT_REFS_MAX)
        .map(|n| format!("{n}{}", "\"".repeat(150)))
        .collect();
    f.h().append_lane_report("lane-1", 1, ready, 3).unwrap();
    let mut r = begin(&f, 1);
    r.caveats = (0..LANE_CAVEATS_MAX)
        .map(|n| format!("{n}{}", "\"".repeat(200)))
        .collect();
    f.h().begin_lane_verification(r.clone()).unwrap();
    let mut request = close_request(&r);
    request.note = "\"é".repeat(LANE_CLOSE_NOTE_MAX_BYTES / 3);
    let closed = f.h().close_lane(request).unwrap();
    let stored = payload(&f, &closed.close_id);
    assert_eq!(stored.artifact_refs.len(), LANE_ARTIFACT_REFS_MAX);
    assert_eq!(stored.review.unwrap().caveats.len(), LANE_CAVEATS_MAX);
    // One byte over the note limit, or an empty note, is an input error.
    let f = Fixture::new();
    started(&f);
    ready_without_artifact(&f);
    let mut long = close_now(&f);
    long.note = "x".repeat(LANE_CLOSE_NOTE_MAX_BYTES + 1);
    rejected(f.h().close_lane(long), LaneOperationError::InvalidInput);
    let mut empty = close_now(&f);
    empty.note = "  ".into();
    rejected(f.h().close_lane(empty), LaneOperationError::InvalidInput);
}

#[test]
fn a_human_close_or_discard_is_recorded_apart_from_a_lead_close() {
    let f = Fixture::new();
    started(&f);
    // No Ready at all: a human can still end the lane's ledger entry.
    let closed = f
        .h()
        .close_lane(human_close(&f, LaneCloseKind::HumanClosed))
        .unwrap();
    assert_eq!(closed.kind, LaneCloseKind::HumanClosed);
    assert_eq!(closed.ready_seq, None);
    assert_eq!(
        closed.closed_by,
        Some(LaneCloseActor::Human {
            surface: "web".into(),
            client_id: Some("client-1".into())
        })
    );
    let discard = f
        .h()
        .close_lane(human_close(&f, LaneCloseKind::Discarded))
        .unwrap();
    // One current close: the first one stands.
    assert_eq!(discard.outcome, LaneCloseOutcome::AlreadyClosed);
    assert_eq!(discard.kind, LaneCloseKind::HumanClosed);
    assert_eq!(f.count("lane_closes"), 1);
    // Nothing stopped or detached: the run, thread and attachment are untouched.
    assert_eq!(
        f.db().lane("lane-1").unwrap().unwrap().policy,
        LanePolicy::Attached
    );
    assert_eq!(
        f.sql()
            .query_row("SELECT kind FROM lane_closes", [], |r| r
                .get::<_, String>(0))
            .unwrap(),
        "human_closed"
    );
}

#[test]
fn a_lead_cannot_claim_a_human_kind_and_a_human_cannot_claim_a_lead_close() {
    let f = Fixture::new();
    started(&f);
    ready_without_artifact(&f);
    for kind in [LaneCloseKind::HumanClosed, LaneCloseKind::Discarded] {
        let mut request = close_now(&f);
        request.kind = kind;
        rejected(f.h().close_lane(request), LaneOperationError::InvalidInput);
    }
    let mut human = human_close(&f, LaneCloseKind::LeadClosed);
    human.ready_seq = None;
    rejected(f.h().close_lane(human), LaneOperationError::InvalidInput);
    let mut no_ready = close_now(&f);
    no_ready.ready_seq = None;
    rejected(f.h().close_lane(no_ready), LaneOperationError::InvalidInput);
    assert_eq!(f.count("lane_closes"), 0);
}

#[test]
fn ready_attention_says_why_it_waits_and_never_withholds_close() {
    let f = Fixture::new();
    started(&f);
    ready_without_artifact(&f);
    let (readiness, actions) = states(&f);
    assert_eq!(readiness, LaneReadyState::NoEvidence);
    assert!(actions.contains(&LaneAttentionAction::Close));
    assert!(actions.contains(&LaneAttentionAction::Verify));
    f.h()
        .append_lane_report("lane-1", 1, report(LaneStatus::Ready), 4)
        .unwrap();
    assert_eq!(states(&f).0, LaneReadyState::Unreviewed);
    let r = begin(&f, 1);
    f.h().begin_lane_verification(r.clone()).unwrap();
    let (readiness, actions) = states(&f);
    assert_eq!(readiness, LaneReadyState::ReviewInProgress);
    assert!(actions.contains(&LaneAttentionAction::Close));
    assert!(!actions.contains(&LaneAttentionAction::Verify));
    let mut rejected_review = submission();
    rejected_review.verdict = LaneVerificationVerdict::Rejected;
    rejected_review.checks[0].passed = false;
    f.h()
        .submit_lane_verification(
            r.verification_id,
            r.reviewer_run_id.clone(),
            rejected_review,
            11,
        )
        .unwrap();
    done(&f, &r.reviewer_run_id, "Reviewer", "Done");
    f.h()
        .complete_lane_verification(r.verification_id, r.reviewer_run_id.clone(), 21)
        .unwrap();
    let (readiness, actions) = states(&f);
    assert_eq!(readiness, LaneReadyState::ReviewRejected);
    assert!(actions.contains(&LaneAttentionAction::Close));
    let approved = verified(&f, 2);
    assert_eq!(states(&f).0, LaneReadyState::AwaitingClose);
    f.h().close_lane(close_request(&approved)).unwrap();
    assert!(
        f.h()
            .lane_attention(filter(), None, 20)
            .unwrap()
            .items
            .is_empty()
    );
}

fn facts() -> storage::pull_requests::PullRequestFacts {
    use storage::pull_requests::*;
    PullRequestFacts {
        state: PullRequestState::Open,
        base_branch: "main".into(),
        head_repository: Some("owner/repo".into()),
        head_branch: "feature".into(),
        head_sha: "a".repeat(40),
        checks: vec![],
        checks_complete: true,
        review_decision: ReviewDecision::Approved,
    }
}
fn link_pr(f: &Fixture) -> storage::pull_requests::PullRequestKey {
    use storage::pull_requests::*;
    let key = PullRequestKey::github("owner/repo", 7).unwrap();
    f.h()
        .link_pull_request(LinkPullRequest {
            thread_id: "thread-1".into(),
            key: key.clone(),
            expected_revision: 0,
            at_ns: 2,
        })
        .unwrap();
    key
}

#[test]
fn a_close_without_a_review_still_records_the_stored_pr_context() {
    use storage::pull_requests::*;
    // Not linked.
    let f = Fixture::new();
    started(&f);
    ready_without_artifact(&f);
    let status = |f: &Fixture| f.h().close_lane(close_now(f)).unwrap().pr_context.unwrap();
    assert_eq!(status(&f).status, LanePrContextStatus::NotLinked);
    // Linked, nothing fetched yet.
    let f = Fixture::new();
    started(&f);
    link_pr(&f);
    ready_without_artifact(&f);
    let context = status(&f);
    assert_eq!(context.status, LanePrContextStatus::NoSnapshot);
    assert_eq!(context.evidence, None);
    // Fetched: the stored evidence is attached, with no fetch from the close.
    let f = Fixture::new();
    started(&f);
    let key = link_pr(&f);
    let seq = f.h().begin_pull_request_fetch(&key).unwrap();
    f.h()
        .commit_pull_request_fetch(&key, seq, facts(), 5)
        .unwrap();
    ready_without_artifact(&f);
    let stored = f.h().lane_pr_evidence("thread-1".into()).unwrap();
    let context = status(&f);
    assert_eq!(context.status, LanePrContextStatus::Available);
    assert_eq!(context.evidence, stored);
    assert_eq!(
        f.h()
            .pull_request_snapshot(&key)
            .unwrap()
            .unwrap()
            .fetch_seq,
        seq
    );
    // The latest fetch failed: the earlier evidence is kept and the failure is named.
    let f = Fixture::new();
    started(&f);
    let key = link_pr(&f);
    let seq = f.h().begin_pull_request_fetch(&key).unwrap();
    f.h()
        .commit_pull_request_fetch(&key, seq, facts(), 5)
        .unwrap();
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
    ready_without_artifact(&f);
    let context = status(&f);
    assert_eq!(context.status, LanePrContextStatus::LastFetchFailed);
    assert!(context.evidence.is_some());
}

#[test]
fn a_reviewer_that_ended_without_a_complete_submission_is_incomplete_not_under_review() {
    let f = Fixture::new();
    started(&f);
    f.h()
        .append_lane_report("lane-1", 1, report(LaneStatus::Ready), 3)
        .unwrap();
    let r = begin(&f, 1);
    f.h().begin_lane_verification(r.clone()).unwrap();
    assert_eq!(states(&f).0, LaneReadyState::ReviewInProgress);
    // The run ended (failed or cancelled) but nothing wrote that back, e.g. a crash:
    // attention derives it from the Reviewer's own record and changes nothing.
    done(&f, &r.reviewer_run_id, "Reviewer", "Error");
    let (readiness, actions) = states(&f);
    assert_eq!(readiness, LaneReadyState::ReviewIncomplete);
    assert!(actions.contains(&LaneAttentionAction::Verify));
    assert!(actions.contains(&LaneAttentionAction::Close));
    assert_eq!(
        f.h()
            .lane_verification(r.verification_id)
            .unwrap()
            .unwrap()
            .state,
        LaneVerificationState::Pending
    );
    // Writing it back is bound to the reserved run and changes only a running review.
    rejected(
        f.h()
            .incomplete_lane_verification(r.verification_id, "reviewer-2".into()),
        LaneOperationError::NotResponder,
    );
    let marked = f
        .h()
        .incomplete_lane_verification(r.verification_id, r.reviewer_run_id.clone())
        .unwrap();
    assert_eq!(marked.state, LaneVerificationState::Incomplete);
    assert_eq!(states(&f).0, LaneReadyState::ReviewIncomplete);
    // An Incomplete review is not a rejection and a close records it as it was.
    let closed = f.h().close_lane(close_request(&r)).unwrap();
    assert_eq!(
        closed.review.unwrap().state,
        LaneVerificationState::Incomplete
    );
}

#[test]
fn a_finished_review_is_never_downgraded_and_a_silent_finish_is_incomplete() {
    let f = Fixture::new();
    started(&f);
    f.h()
        .append_lane_report("lane-1", 1, report(LaneStatus::Ready), 3)
        .unwrap();
    // Submitted and ended normally, but the completion write was lost (crash): the
    // submission stands, so it is shown by what it says, not as running or incomplete.
    let r = begin(&f, 1);
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
    assert_eq!(states(&f).0, LaneReadyState::AwaitingClose);
    f.h()
        .complete_lane_verification(r.verification_id, r.reviewer_run_id.clone(), 21)
        .unwrap();
    let unchanged = f
        .h()
        .incomplete_lane_verification(r.verification_id, r.reviewer_run_id.clone())
        .unwrap();
    assert_eq!(unchanged.state, LaneVerificationState::Completed);
    // A Reviewer that finished normally without submitting is incomplete.
    let silent = begin(&f, 2);
    f.h().begin_lane_verification(silent.clone()).unwrap();
    done(&f, &silent.reviewer_run_id, "Reviewer", "Done");
    assert_eq!(states(&f).0, LaneReadyState::ReviewIncomplete);
}

#[test]
fn a_close_records_a_reviewer_that_already_ended_as_it_ended_not_as_running() {
    // Stopped by the operator (resumable, so never written back) before it submitted.
    let f = Fixture::new();
    started(&f);
    f.h()
        .append_lane_report("lane-1", 1, report(LaneStatus::Ready), 3)
        .unwrap();
    let r = begin(&f, 1);
    f.h().begin_lane_verification(r.clone()).unwrap();
    done(&f, &r.reviewer_run_id, "Reviewer", "Stopped");
    let closed = f.h().close_lane(close_request(&r)).unwrap();
    assert_eq!(
        closed.review.unwrap().state,
        LaneVerificationState::Incomplete
    );
    // The review itself is untouched: only the close's reference is derived.
    assert_eq!(
        f.h()
            .lane_verification(r.verification_id)
            .unwrap()
            .unwrap()
            .state,
        LaneVerificationState::Pending
    );
    // Submitted, then ended normally, with the completion write lost: it is recorded by
    // what it says.
    let f = Fixture::new();
    started(&f);
    f.h()
        .append_lane_report("lane-1", 1, report(LaneStatus::Ready), 3)
        .unwrap();
    let r = begin(&f, 1);
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
    let review = f.h().close_lane(close_request(&r)).unwrap().review.unwrap();
    assert_eq!(review.state, LaneVerificationState::Completed);
    assert_eq!(review.verdict, Some(LaneVerificationVerdict::Approved));
    // A Reviewer still running (only a checkpoint on record) is recorded as running.
    let f = Fixture::new();
    started(&f);
    f.h()
        .append_lane_report("lane-1", 1, report(LaneStatus::Ready), 3)
        .unwrap();
    let r = begin(&f, 1);
    f.h().begin_lane_verification(r.clone()).unwrap();
    done(&f, &r.reviewer_run_id, "Reviewer", "Checkpoint");
    assert_eq!(
        f.h()
            .close_lane(close_request(&r))
            .unwrap()
            .review
            .unwrap()
            .state,
        LaneVerificationState::Pending
    );
}
