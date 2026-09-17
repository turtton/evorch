use event_bus::orchestrator::CriterionEvidence;
use event_bus::{CriterionCheck, CriterionStatus, ReviewVerdict};
use runtime::orchestration::review::{ReviewLoop, ReviewOutcome, ReviewResult};

fn valid() -> CriterionCheck {
    CriterionCheck {
        id: "AC1".into(),
        status: CriterionStatus::Met,
        note: "verified".into(),
        evidence: Some(CriterionEvidence {
            command: "cargo test".into(),
            exit_status: 0,
            target_sha: "head".into(),
            diff_ref: Some("base..head".into()),
            artifact_path: Some("green.log".into()),
            red_evidence: Some("red.log".into()),
        }),
    }
}

fn rejects(criteria: Vec<CriterionCheck>) {
    // Given: an approval whose evidence does not satisfy the contract.
    let mut review = ReviewLoop::new(3);
    // When: the typed result enters the bounded review loop.
    let outcome = review.on_review_result(
        ReviewResult {
            verdict: ReviewVerdict::Approve,
            criteria,
        },
        "head",
        "review-1",
    );
    // Then: it requests repair, rather than issuing a merge binding.
    assert!(
        matches!(outcome, ReviewOutcome::Repair { round: 1, .. }),
        "{outcome:?}"
    );
    assert!(!outcome.can_issue_merge_binding());
}

#[test]
fn approval_rejects_empty_checklist() {
    rejects(vec![]);
}

#[test]
fn approval_rejects_missing_evidence() {
    let mut check = valid();
    check.evidence = None;
    rejects(vec![check]);
}

#[test]
fn approval_rejects_nonzero_exit() {
    let mut check = valid();
    check.evidence.as_mut().unwrap().exit_status = 1;
    rejects(vec![check]);
}

#[test]
fn approval_rejects_mismatched_sha() {
    let mut check = valid();
    check.evidence.as_mut().unwrap().target_sha = "other".into();
    rejects(vec![check]);
}

#[test]
fn approval_rejects_blank_command() {
    let mut check = valid();
    check.evidence.as_mut().unwrap().command = " \n".into();
    rejects(vec![check]);
}

#[test]
fn approval_rejects_missing_diff() {
    let mut check = valid();
    check.evidence.as_mut().unwrap().diff_ref = None;
    rejects(vec![check]);
}

#[test]
fn approval_rejects_blank_artifact() {
    let mut check = valid();
    check.evidence.as_mut().unwrap().artifact_path = Some(" ".into());
    rejects(vec![check]);
}

#[test]
fn approval_rejects_missing_red() {
    let mut check = valid();
    check.evidence.as_mut().unwrap().red_evidence = None;
    rejects(vec![check]);
}
