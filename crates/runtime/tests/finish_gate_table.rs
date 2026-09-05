use event_bus::{CiState, CriterionCheck, CriterionStatus, GateRejection, ReviewVerdict};
use evorch_runtime::orchestration::gate::{
    CiEvidence, CriteriaEvidence, GateInputs, GateVerdict, PullRequestEvidence, ReviewEvidence,
    evaluate,
};
use runtime as evorch_runtime;

const HEAD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const OLD_HEAD: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

#[derive(Clone)]
struct Fixture {
    branch: Option<String>,
    current_head: Option<String>,
    pr: Option<PullRequestEvidence>,
    ci: Option<CiEvidence>,
    criteria: Option<CriteriaEvidence>,
    review: Option<ReviewEvidence>,
    rounds: u32,
    max_rounds: u32,
}

impl Fixture {
    fn passing() -> Self {
        Self {
            branch: Some("evorch/task/run-1".into()),
            current_head: Some(HEAD.into()),
            pr: Some(PullRequestEvidence {
                repo: "turtton/evorch".into(),
                number: 73,
                url: "https://github.com/turtton/evorch/pull/73".into(),
                base_ref: "main".into(),
                head_sha: HEAD.into(),
            }),
            ci: Some(CiEvidence {
                head_sha: HEAD.into(),
                state: CiState::Green,
            }),
            criteria: Some(CriteriaEvidence {
                head_sha: HEAD.into(),
                reviewer_run_id: "review-1".into(),
                round: 1,
                checklist: vec![CriterionCheck {
                    id: "ac1".into(),
                    status: CriterionStatus::Met,
                    note: "verified".into(),
                }],
            }),
            review: Some(ReviewEvidence {
                head_sha: HEAD.into(),
                reviewer_run_id: "review-1".into(),
                round: 1,
                verdict: ReviewVerdict::Approve,
            }),
            rounds: 1,
            max_rounds: 3,
        }
    }

    fn evaluate(&self) -> GateVerdict {
        evaluate(&GateInputs {
            expected_repo: "turtton/evorch",
            expected_base: "main",
            deliverable_branch: self.branch.as_deref(),
            current_head: self.current_head.as_deref(),
            pr: self.pr.as_ref(),
            ci: self.ci.as_ref(),
            criteria: self.criteria.as_ref(),
            review: self.review.as_ref(),
            review_rounds_used: self.rounds,
            max_review_rounds: self.max_rounds,
        })
    }
}

fn assert_reject(fixture: Fixture, expected: Vec<GateRejection>) {
    let before = fixture.clone();
    assert_eq!(fixture.evaluate(), GateVerdict::Reject(expected));
    assert_eq!(fixture.branch, before.branch);
    assert_eq!(fixture.current_head, before.current_head);
    assert_eq!(fixture.pr, before.pr);
    assert_eq!(fixture.ci, before.ci);
    assert_eq!(fixture.criteria, before.criteria);
    assert_eq!(fixture.review, before.review);
}

#[test]
fn no_branch() {
    let mut f = Fixture::passing();
    f.branch = None;
    assert_reject(f, vec![GateRejection::NoDeliverableBranch]);
}

#[test]
fn no_pr() {
    let mut f = Fixture::passing();
    f.pr = None;
    assert_reject(f, vec![GateRejection::NoPullRequest]);
}

#[test]
fn wrong_repo() {
    let mut f = Fixture::passing();
    f.pr.as_mut().expect("PR fixture").repo = "other/repo".into();
    assert_reject(
        f,
        vec![GateRejection::PullRequestRepoMismatch {
            expected: "turtton/evorch".into(),
            actual: "other/repo".into(),
        }],
    );
}

#[test]
fn wrong_base() {
    let mut f = Fixture::passing();
    f.pr.as_mut().expect("PR fixture").base_ref = "develop".into();
    assert_reject(
        f,
        vec![GateRejection::PullRequestBaseMismatch {
            expected: "main".into(),
            actual: "develop".into(),
        }],
    );
}

#[test]
fn stale_pr_head() {
    let mut f = Fixture::passing();
    f.pr.as_mut().expect("PR fixture").head_sha = OLD_HEAD.into();
    assert_reject(
        f,
        vec![GateRejection::StaleHead {
            evidence: "pull_request".into(),
            evidence_head: OLD_HEAD.into(),
            current_head: HEAD.into(),
        }],
    );
}

#[test]
fn ci_missing() {
    let mut f = Fixture::passing();
    f.ci = None;
    assert_reject(
        f,
        vec![GateRejection::CiMissing {
            head_sha: HEAD.into(),
        }],
    );
}

#[test]
fn ci_pending() {
    let mut f = Fixture::passing();
    f.ci.as_mut().expect("CI fixture").state = CiState::Pending;
    assert_reject(
        f,
        vec![GateRejection::CiPending {
            head_sha: HEAD.into(),
        }],
    );
}

#[test]
fn ci_failing() {
    let mut f = Fixture::passing();
    f.ci.as_mut().expect("CI fixture").state = CiState::Failing {
        summary: "tests failed".into(),
    };
    assert_reject(
        f,
        vec![GateRejection::CiFailing {
            head_sha: HEAD.into(),
            summary: "tests failed".into(),
        }],
    );
}

#[test]
fn criteria_unverified() {
    let mut f = Fixture::passing();
    f.criteria = None;
    assert_reject(
        f,
        vec![GateRejection::CriteriaUnverified {
            head_sha: HEAD.into(),
        }],
    );
}

#[test]
fn criteria_unknown_never_passes() {
    let mut f = Fixture::passing();
    let check = &mut f.criteria.as_mut().expect("criteria fixture").checklist[0];
    check.status = CriterionStatus::Unknown;
    assert_reject(
        f,
        vec![GateRejection::CriteriaUnmet {
            head_sha: HEAD.into(),
            ids: vec!["ac1".into()],
        }],
    );
}

#[test]
fn criteria_unmet() {
    let mut f = Fixture::passing();
    let check = &mut f.criteria.as_mut().expect("criteria fixture").checklist[0];
    check.status = CriterionStatus::Unmet;
    assert_reject(
        f,
        vec![GateRejection::CriteriaUnmet {
            head_sha: HEAD.into(),
            ids: vec!["ac1".into()],
        }],
    );
}

#[test]
fn review_missing() {
    let mut f = Fixture::passing();
    f.review = None;
    assert_reject(
        f,
        vec![GateRejection::ReviewMissing {
            head_sha: HEAD.into(),
        }],
    );
}

#[test]
fn review_request_update() {
    let mut f = Fixture::passing();
    f.review.as_mut().expect("review fixture").verdict = ReviewVerdict::RequestUpdate {
        findings: vec!["fix tests".into()],
    };
    assert_reject(
        f,
        vec![GateRejection::ReviewRequestUpdate {
            head_sha: HEAD.into(),
            round: 1,
        }],
    );
}

#[test]
fn review_stale_head() {
    let mut f = Fixture::passing();
    f.review.as_mut().expect("review fixture").head_sha = OLD_HEAD.into();
    assert_reject(
        f,
        vec![GateRejection::ReviewStale {
            reviewed_head: OLD_HEAD.into(),
            current_head: HEAD.into(),
        }],
    );
}

#[test]
fn rounds_exhausted() {
    let mut f = Fixture::passing();
    f.rounds = 3;
    assert_reject(f, vec![GateRejection::ReviewRoundsExhausted { rounds: 3 }]);
}

#[test]
fn all_pass_accepts_with_snapshot() {
    let f = Fixture::passing();
    let GateVerdict::Accept(snapshot) = f.evaluate() else {
        panic!("passing evidence must be accepted");
    };
    assert_eq!(snapshot.repo, "turtton/evorch");
    assert_eq!(snapshot.pr_number, 73);
    assert_eq!(snapshot.base_ref, "main");
    assert_eq!(snapshot.head_sha, HEAD);
    assert_eq!(snapshot.ci, CiState::Green);
    assert_eq!(snapshot.criteria_round, 1);
    assert_eq!(snapshot.review_round, 1);
    assert_eq!(snapshot.reviewer_run_id, "review-1");
}
