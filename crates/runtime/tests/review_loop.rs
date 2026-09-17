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
fn approval_requires_valid_evidence_with_at_least_one_reference() {
    // Given: each evidence field is varied independently, including reference alternatives.
    let all = [Some("diff"), Some("log"), Some("red")];
    for (command, exit_status, sha, references, approved) in [
        ("test", 0, HEAD_A, [Some("diff"), None, None], true),
        ("test", 0, HEAD_A, [None, Some("artifact"), None], true),
        ("test", 0, HEAD_A, [None, None, Some("red")], true),
        ("", 0, HEAD_A, all, false),
        (" \t", 0, HEAD_A, all, false),
        ("test", 1, HEAD_A, all, false),
        ("test", 0, HEAD_B, all, false),
        ("test", 0, HEAD_A, [None, None, None], false),
        (
            "test",
            0,
            HEAD_A,
            [Some(""), Some(" \t"), Some("\n")],
            false,
        ),
    ] {
        let [diff_ref, artifact_path, red_evidence] =
            references.map(|value| value.map(str::to_owned));
        let result = ReviewResult {
            verdict: ReviewVerdict::Approve,
            criteria: vec![event_bus::CriterionCheck {
                id: "AC1".into(),
                status: CriterionStatus::Met,
                note: "checked".into(),
                evidence: Some(CriterionEvidence {
                    command: command.into(),
                    exit_status,
                    target_sha: sha.into(),
                    diff_ref,
                    artifact_path,
                    red_evidence,
                }),
            }],
        };
        // When: the actual review loop evaluates the typed evidence.
        let outcome = ReviewLoop::new(3).on_review_result(result, HEAD_A, "review-1");
        // Then: only valid current-head evidence permits approval.
        assert_eq!(
            outcome.can_issue_merge_binding(),
            approved,
            "{command:?}, {exit_status}, {sha}, {references:?}"
        );
        if !approved {
            assert!(matches!(outcome, ReviewOutcome::Repair { .. }));
            assert!(matches!(&outcome.evidence().review, GateEvidence::Review {
                verdict: ReviewVerdict::RequestUpdate { findings }, ..
            } if findings.iter().any(|finding| finding.contains("AC1"))));
        }
    }
}

#[test]
fn approve_with_missing_evidence_requests_update() {
    // Given: a Met criterion without execution evidence.
    let result = ReviewResult {
        verdict: ReviewVerdict::Approve,
        criteria: vec![event_bus::CriterionCheck {
            id: "AC1".into(),
            status: CriterionStatus::Met,
            note: "checked".into(),
            evidence: None,
        }],
    };
    // When: the typed result enters the existing review loop.
    let outcome = ReviewLoop::new(3).on_review_result(result, HEAD_A, "review-1");
    // Then: the missing evidence produces a concrete repair finding.
    assert!(matches!(outcome, ReviewOutcome::Repair { findings, .. }
        if findings == ["acceptance criteria not met: AC1"]));
}

#[test]
fn approve_with_empty_checklist_requests_update() {
    // Given: an approval with no acceptance criteria.
    let result = ReviewResult {
        verdict: ReviewVerdict::Approve,
        criteria: vec![],
    };
    // When: the typed result enters the existing review loop.
    let outcome = ReviewLoop::new(3).on_review_result(result, HEAD_A, "review-1");
    // Then: an empty checklist cannot authorize approval.
    assert!(matches!(outcome, ReviewOutcome::Repair { findings, .. }
        if findings == ["acceptance criteria checklist is empty"]));
}

#[test]
fn supervisor_prefers_typed_tool_result_over_prose() {
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
            evidence: Some(evidence()),
        }]
    );
}

fn review(verdict: &str, findings: &[&str], status: &str) -> String {
    serde_json::json!({
        "verdict": verdict,
        "findings": findings,
        "criteria": [{"id": "AC1", "status": status, "note": "checked", "evidence": evidence()}]
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
        &format!(
            "```json\n{}\n```",
            review("approve", &[], "met").replace(HEAD_A, HEAD_B)
        ),
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
