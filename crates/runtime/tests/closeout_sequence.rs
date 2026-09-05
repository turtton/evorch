use event_bus::CloseoutStep;
use runtime::orchestration::DeliveryError;

const HEAD_A: &str = "a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1";
const HEAD_B: &str = "a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2";
use runtime::orchestration::closeout::{CloseoutStatus, run_closeout};
use runtime::orchestration::delivery::{DeliveryCall, FixtureDeliveryAdapter};

#[tokio::test]
async fn closeout_runs_claim_then_summary_then_complete_after_merge_only() {
    let delivery = FixtureDeliveryAdapter::default();
    delivery.script_closeout(Ok(Some("claim-ref".into())));
    delivery.script_closeout(Ok(Some("summary-ref".into())));
    delivery.script_closeout(Ok(None));

    let before_merge = run_closeout(&delivery, "goal-1", false).await;
    let after_merge = run_closeout(&delivery, "goal-1", true).await;

    assert_eq!(before_merge.status, CloseoutStatus::NotMerged);
    assert_eq!(after_merge.status, CloseoutStatus::Complete);
    assert_eq!(
        after_merge
            .records
            .iter()
            .map(|record| record.step)
            .collect::<Vec<_>>(),
        vec![
            CloseoutStep::WorkerClaim,
            CloseoutStep::ResultSummary,
            CloseoutStep::WorkerComplete,
        ]
    );
}

#[tokio::test]
async fn closeout_failure_is_recorded_and_blocks_not_completes() {
    let delivery = FixtureDeliveryAdapter::default();
    delivery.script_closeout(Ok(Some("claim-ref".into())));
    delivery.script_closeout(Err(DeliveryError::Command("summary failed".into())));

    let result = run_closeout(&delivery, "goal-1", true).await;

    assert_eq!(result.status, CloseoutStatus::Blocked);
    assert_eq!(result.records.len(), 2);
    assert!(result.records[0].ok);
    assert!(!result.records[1].ok);
    assert_eq!(
        result.records[1].detail,
        "delivery command failed: summary failed"
    );
}

#[tokio::test]
async fn closeout_never_calls_queue_or_publish() {
    let delivery = FixtureDeliveryAdapter::default();
    delivery.script_closeout(Ok(None));
    delivery.script_closeout(Ok(None));
    delivery.script_closeout(Ok(None));

    let result = run_closeout(&delivery, "goal-1", true).await;

    assert_eq!(result.status, CloseoutStatus::Complete);
    assert!(delivery.recorded().iter().all(|call| matches!(
        call,
        DeliveryCall::CloseoutStep {
            step: CloseoutStep::WorkerClaim
                | CloseoutStep::ResultSummary
                | CloseoutStep::WorkerComplete,
            ..
        }
    )));
}

#[tokio::test]
async fn scripted_happy_path_delivers_two_heads_merge_and_closeout() {
    use event_bus::{CiState, GateEvidence, GateSnapshot};
    use runtime::orchestration::DeliveryPort;
    use runtime::orchestration::approval::MergeApprovals;

    let delivery = FixtureDeliveryAdapter::scripted_happy_path();
    delivery.push_branch("evorch/task/run-1").await.unwrap();
    let created = delivery
        .find_or_create_pr("evorch/task/run-1", "main", "title", "body")
        .await
        .unwrap();
    let first_ci = delivery.ci_status("turtton/evorch", HEAD_A).await.unwrap();
    delivery.push_branch("evorch/task/run-1").await.unwrap();
    let second_pr = delivery.pr_status("turtton/evorch", 101).await.unwrap();
    let second_ci = delivery.ci_status("turtton/evorch", HEAD_B).await.unwrap();
    let snapshot = GateSnapshot {
        repo: "turtton/evorch".into(),
        pr_number: 101,
        base_ref: "main".into(),
        head_sha: HEAD_B.into(),
        ci: CiState::Green,
        criteria_round: 2,
        review_round: 2,
        reviewer_run_id: "review-2".into(),
    };
    let mut approvals = MergeApprovals::default();
    let binding = approvals.issue("goal-1", snapshot.clone(), &|| [7; 16]);
    approvals.approve(&binding.token_id, &snapshot).unwrap();
    let approved = approvals.consume(&binding.token_id).unwrap();
    delivery.merge_pr(&approved).await.unwrap();
    let closeout = run_closeout(&delivery, "goal-1", true).await;

    assert!(matches!(created, GateEvidence::PullRequest { head_sha, .. } if head_sha == HEAD_A));
    assert!(
        matches!(first_ci, GateEvidence::Ci { head_sha, state: CiState::Green } if head_sha == HEAD_A)
    );
    assert!(matches!(second_pr, GateEvidence::PullRequest { head_sha, .. } if head_sha == HEAD_B));
    assert!(
        matches!(second_ci, GateEvidence::Ci { head_sha, state: CiState::Green } if head_sha == HEAD_B)
    );
    assert_eq!(closeout.status, CloseoutStatus::Complete);
}
