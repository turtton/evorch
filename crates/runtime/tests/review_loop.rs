use event_bus::{CriterionStatus, GateEvidence, ReviewVerdict};
use runtime::orchestration::review::{ReviewLoop, ReviewOutcome};

const HEAD_A: &str = "a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1";
const HEAD_B: &str = "a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2";

fn review(verdict: &str, findings: &[&str], status: &str) -> String {
    serde_json::json!({
        "verdict": verdict,
        "findings": findings,
        "criteria": [{"id": "AC1", "status": status, "note": "checked"}]
    })
    .to_string()
}

#[test]
fn request_update_then_approve_converges_in_two_rounds() {
    let mut review_loop = ReviewLoop::new(3);

    let first = review_loop.on_reviewer_done(
        &format!(
            "final\n```json\n{}\n```",
            review("request-update", &["fix it"], "unmet")
        ),
        HEAD_A,
        "review-1",
    );
    let second = review_loop.on_reviewer_done(
        &format!("```json\n{}\n```", review("approve", &[], "met")),
        HEAD_B,
        "review-2",
    );

    assert!(matches!(first, ReviewOutcome::Repair { round: 1, .. }));
    assert!(matches!(second, ReviewOutcome::Approve { round: 2, .. }));
    assert_eq!(review_loop.rounds_used(), 2);
}

#[test]
fn rounds_exhausted_blocks_and_never_issues_merge_binding() {
    let mut review_loop = ReviewLoop::new(2);

    let first = review_loop.on_reviewer_done(
        &format!(
            "```json\n{}\n```",
            review("request-update", &["one"], "unmet")
        ),
        HEAD_A,
        "review-1",
    );
    let second = review_loop.on_reviewer_done(
        &format!(
            "```json\n{}\n```",
            review("request-update", &["two"], "unmet")
        ),
        HEAD_B,
        "review-2",
    );

    assert!(matches!(first, ReviewOutcome::Repair { .. }));
    assert!(
        matches!(second, ReviewOutcome::Blocked { ref reason, .. } if reason == "review rounds exhausted")
    );
    assert!(!second.can_issue_merge_binding());
}

#[test]
fn unparsable_reviewer_output_is_request_update_not_approve() {
    let mut review_loop = ReviewLoop::new(3);

    let outcome = review_loop.on_reviewer_done("looks good", HEAD_A, "review-1");

    assert!(matches!(
        outcome,
        ReviewOutcome::Repair { findings, .. }
            if findings == vec!["reviewer output unparsable"]
    ));
}

#[test]
fn repeated_identical_findings_block() {
    let mut review_loop = ReviewLoop::new(3);
    let result = format!(
        "```json\n{}\n```",
        review("request-update", &["same finding"], "unmet")
    );

    let first = review_loop.on_reviewer_done(&result, HEAD_A, "review-1");
    let second = review_loop.on_reviewer_done(&result, HEAD_B, "review-2");

    assert!(matches!(first, ReviewOutcome::Repair { .. }));
    assert!(
        matches!(second, ReviewOutcome::Blocked { reason, .. } if reason == "review findings repeated")
    );
}

#[test]
fn criteria_unknown_recorded_as_unmet_evidence() {
    let mut review_loop = ReviewLoop::new(3);

    let outcome = review_loop.on_reviewer_done(
        &format!("```json\n{}\n```", review("approve", &[], "unknown")),
        HEAD_A,
        "review-1",
    );

    let evidence = outcome.evidence();
    assert!(matches!(
        &evidence.criteria,
        GateEvidence::Criteria { checklist, .. }
            if checklist[0].status == CriterionStatus::Unknown
    ));
    assert!(matches!(
        &evidence.review,
        GateEvidence::Review { verdict: ReviewVerdict::RequestUpdate { findings }, .. }
            if findings == &vec!["acceptance criteria not met: AC1".to_string()]
    ));
    assert!(matches!(outcome, ReviewOutcome::Repair { .. }));
}
