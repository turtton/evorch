use event_bus::ReviewVerdict;
use runtime::orchestration::review::{ReviewLoop, ReviewOutcome, ReviewResult};

#[test]
fn exhausted_restored_review_cannot_approve() {
    let mut review = ReviewLoop::restore(3, 3, None);
    let result = serde_json::from_value(serde_json::json!({
        "verdict":"approve", "criteria":[{"id":"AC1", "status":"met", "note":"checked", "evidence":{
            "command":"cargo test", "exit_status":0, "target_sha":"head",
            "diff_ref":"base..head", "artifact_path":"green.log", "red_evidence":"red.log"
        }}]
    }))
    .unwrap();
    let outcome = review.on_review_result(result, "head", "review-4");
    assert!(
        matches!(outcome, ReviewOutcome::Blocked { reason, .. } if reason == "review rounds exhausted")
    );
}

#[test]
fn restored_review_preserves_round_limit() {
    // Given
    let mut review = ReviewLoop::restore(3, 2, None);
    // When
    let outcome = review.on_review_result(
        ReviewResult {
            verdict: ReviewVerdict::RequestUpdate {
                findings: vec!["repair".into()],
            },
            criteria: vec![],
        },
        "head",
        "review-3",
    );
    // Then
    assert!(
        matches!(outcome, ReviewOutcome::Blocked { round: 3, reason, .. } if reason == "review rounds exhausted")
    );
}

#[test]
fn restored_review_preserves_repeated_findings() {
    // Given
    let mut review = ReviewLoop::restore(3, 1, Some(vec!["same".into()]));
    // When
    let outcome = review.on_review_result(
        ReviewResult {
            verdict: ReviewVerdict::RequestUpdate {
                findings: vec!["same".into()],
            },
            criteria: vec![],
        },
        "head",
        "review-2",
    );
    // Then
    assert!(
        matches!(outcome, ReviewOutcome::Blocked { round: 2, reason, .. } if reason == "review findings repeated")
    );
}
