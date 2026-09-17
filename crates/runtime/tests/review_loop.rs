use event_bus::orchestrator::CriterionEvidence;
use event_bus::{CriterionStatus, GateEvidence, ReviewVerdict};
use runtime::orchestration::review::{
    ReviewLoop, ReviewOutcome, ReviewResult, parse_reviewer_output,
};

const HEAD_A: &str = "a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1";
const HEAD_B: &str = "a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2";

fn evidence() -> CriterionEvidence {
    CriterionEvidence {
        command: "cargo test -p runtime --test review_loop".into(),
        exit_status: 0,
        target_sha: HEAD_A.into(),
        diff_ref: Some("HEAD~1..HEAD".into()),
        artifact_path: Some("artifacts/review.log".into()),
        red_evidence: Some("before: missing criterion evidence".into()),
    }
}

#[test]
fn typed_criterion_evidence_round_trips_into_gate_evidence() {
    // Given: structured approval conflicts with the prose fallback.
    let typed: ReviewResult = serde_json::from_value(serde_json::json!({
        "verdict": "approve",
        "criteria": [{"id": "AC1", "status": "met", "note": "checked", "evidence": evidence()}]
    }))
    .unwrap();
    let mut review_loop = ReviewLoop::new(3);
    // When
    let parsed = parse_reviewer_output(
        Some(typed),
        "```json\n{\"verdict\":\"request-update\",\"findings\":[\"wrong fallback\"]}\n```",
    )
    .unwrap();
    let outcome = review_loop.on_review_result(parsed, HEAD_A, "review-typed");
    // Then
    assert!(matches!(outcome, ReviewOutcome::Approve { round: 1, .. }));
    let expected = GateEvidence::Criteria {
        head_sha: HEAD_A.into(),
        reviewer_run_id: "review-typed".into(),
        round: 1,
        checklist: vec![event_bus::CriterionCheck {
            id: "AC1".into(),
            status: CriterionStatus::Met,
            note: "checked".into(),
            evidence: Some(evidence()),
        }],
    };
    assert_eq!(outcome.evidence().criteria, expected);
    let encoded = serde_json::to_string(&outcome.evidence().criteria).unwrap();
    assert_eq!(
        serde_json::from_str::<GateEvidence>(&encoded).unwrap(),
        expected
    );
}

#[test]
fn approve_with_unmet_and_evidence_forces_request_update_with_evidence() {
    // Given
    let raw = serde_json::json!({"verdict": "approve", "criteria": [
        {"id": "AC1", "status": "unmet", "note": "failed", "evidence": evidence()}
    ]})
    .to_string();
    let mut review_loop = ReviewLoop::new(3);
    // When
    let outcome = review_loop.on_reviewer_done(&raw, HEAD_A, "review-1");
    // Then
    assert!(matches!(&outcome, ReviewOutcome::Repair { findings, .. }
        if findings == &["acceptance criteria not met: AC1"]));
    assert!(
        matches!(&outcome.evidence().criteria, GateEvidence::Criteria { checklist, .. }
        if checklist[0].evidence == Some(evidence()))
    );
    assert!(matches!(&outcome.evidence().review, GateEvidence::Review {
        verdict: ReviewVerdict::RequestUpdate { findings }, ..
    } if findings == &["acceptance criteria not met: AC1"]));
}

#[test]
fn fallback_to_prose_when_typed_absent() {
    // Given
    let prose = format!(
        "Reviewed the change.\n```json\n{}\n```\nDone.",
        review("approve", &[], "met")
    );
    // When
    let parsed = parse_reviewer_output(None, &prose).unwrap();
    // Then
    assert_eq!(parsed.verdict, ReviewVerdict::Approve);
    assert_eq!(
        parsed.criteria,
        vec![event_bus::CriterionCheck {
            id: "AC1".into(),
            status: CriterionStatus::Met,
            note: "checked".into(),
            evidence: None,
        }]
    );
}

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
