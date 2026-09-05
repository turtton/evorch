use event_bus::{CiState, GateSnapshot, InvalidationReason};
use runtime::orchestration::approval::{ApprovalError, ApprovalStatus, MergeApprovals};
use runtime::orchestration::{ApprovedMerge, DeliveryPort};

const HEAD_A: &str = "a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1";
const HEAD_B: &str = "a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2";

fn snapshot(head_sha: &str) -> GateSnapshot {
    GateSnapshot {
        repo: "turtton/evorch".into(),
        pr_number: 101,
        base_ref: "main".into(),
        head_sha: head_sha.into(),
        ci: CiState::Green,
        criteria_round: 2,
        review_round: 2,
        reviewer_run_id: "review-2".into(),
    }
}

fn fixed_rng() -> [u8; 16] {
    [0xab; 16]
}

#[test]
fn approve_requires_matching_head_ci_review_snapshot() {
    let mut approvals = MergeApprovals::default();
    let binding = approvals.issue("goal-1", snapshot(HEAD_A), &fixed_rng);
    let mut changed = snapshot(HEAD_A);
    changed.review_round = 3;

    let error = approvals.approve(&binding.token_id, &changed).unwrap_err();

    assert_eq!(
        error,
        ApprovalError::SnapshotMismatch(InvalidationReason::ReviewChanged)
    );
}

#[test]
fn head_change_invalidates_pending_token() {
    let mut approvals = MergeApprovals::default();
    let binding = approvals.issue("goal-1", snapshot(HEAD_A), &fixed_rng);

    let error = approvals
        .approve(&binding.token_id, &snapshot(HEAD_B))
        .unwrap_err();

    assert_eq!(
        error,
        ApprovalError::SnapshotMismatch(InvalidationReason::HeadChanged {
            from: HEAD_A.into(),
            to: HEAD_B.into(),
        })
    );
    assert_eq!(
        approvals.status(&binding.token_id),
        Some(ApprovalStatus::Invalidated(
            InvalidationReason::HeadChanged {
                from: HEAD_A.into(),
                to: HEAD_B.into(),
            }
        ))
    );
}

#[test]
fn consumed_token_cannot_approve_twice() {
    let mut approvals = MergeApprovals::default();
    let binding = approvals.issue("goal-1", snapshot(HEAD_A), &fixed_rng);
    approvals
        .approve(&binding.token_id, &snapshot(HEAD_A))
        .unwrap();
    let approved = approvals.consume(&binding.token_id).unwrap();

    let error = approvals
        .approve(&binding.token_id, &snapshot(HEAD_A))
        .unwrap_err();

    assert_eq!(approved.binding(), &binding);
    assert_eq!(
        error,
        ApprovalError::Invalidated(InvalidationReason::Consumed)
    );
}

#[test]
fn reject_records_reason_and_invalidates() {
    let mut approvals = MergeApprovals::default();
    let binding = approvals.issue("goal-1", snapshot(HEAD_A), &fixed_rng);

    approvals
        .reject(&binding.token_id, "needs operator review")
        .unwrap();

    assert_eq!(
        approvals.status(&binding.token_id),
        Some(ApprovalStatus::Rejected {
            reason: "needs operator review".into(),
        })
    );
    assert_eq!(
        approvals.consume(&binding.token_id).unwrap_err(),
        ApprovalError::Invalidated(InvalidationReason::Rejected)
    );
}

#[test]
fn approved_merge_type_is_only_way_into_merge_pr() {
    fn merge_accepts_only_approved<T: DeliveryPort + ?Sized>(port: &T, approved: &ApprovedMerge) {
        let _future = port.merge_pr(approved);
    }

    let _ = merge_accepts_only_approved::<runtime::orchestration::FixtureDeliveryAdapter>;
}
