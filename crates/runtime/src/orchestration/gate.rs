//! finish を受理できるかを、SHA 束縛された証跡だけから判定する純粋関数。

use event_bus::{
    CiState, CriterionCheck, CriterionStatus, GateRejection, GateSnapshot, ReviewVerdict,
};

/// Pull Request の gate 証跡。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequestEvidence {
    /// リポジトリ (`owner/repo`)。
    pub repo: String,
    /// PR 番号。
    pub number: u64,
    /// PR URL。
    pub url: String,
    /// マージ先ブランチ。
    pub base_ref: String,
    /// PR の head SHA。
    pub head_sha: String,
}

/// CI の gate 証跡。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CiEvidence {
    /// 観測対象の head SHA。
    pub head_sha: String,
    /// CI 状態。
    pub state: CiState,
}

/// 受け入れ基準の gate 証跡。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CriteriaEvidence {
    /// 検査対象の head SHA。
    pub head_sha: String,
    /// reviewer run ID。
    pub reviewer_run_id: String,
    /// レビューラウンド。
    pub round: u32,
    /// 基準ごとの検査結果。
    pub checklist: Vec<CriterionCheck>,
}

/// reviewer 判定の gate 証跡。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewEvidence {
    /// 判定対象の head SHA。
    pub head_sha: String,
    /// reviewer run ID。
    pub reviewer_run_id: String,
    /// レビューラウンド。
    pub round: u32,
    /// reviewer の判定。
    pub verdict: ReviewVerdict,
}

/// finish gate の全入力。
#[derive(Debug, Clone, Copy)]
pub struct GateInputs<'a> {
    /// 期待するリポジトリ。
    pub expected_repo: &'a str,
    /// 期待するベースブランチ。
    pub expected_base: &'a str,
    /// goal に束縛されたデリバラブルブランチ。
    pub deliverable_branch: Option<&'a str>,
    /// リモートから取得した現在 head。
    pub current_head: Option<&'a str>,
    /// 最新の PR 証跡。
    pub pr: Option<&'a PullRequestEvidence>,
    /// 最新の CI 証跡。
    pub ci: Option<&'a CiEvidence>,
    /// 最新の基準検査証跡。
    pub criteria: Option<&'a CriteriaEvidence>,
    /// 最新のレビュー証跡。
    pub review: Option<&'a ReviewEvidence>,
    /// 使用済みレビューラウンド数。
    pub review_rounds_used: u32,
    /// レビューラウンド上限。
    pub max_review_rounds: u32,
}

/// finish gate の判定結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateVerdict {
    /// 全条件を満たし、証跡スナップショットを固定した。
    Accept(GateSnapshot),
    /// 条件を満たさず、該当理由を仕様順ですべて返した。
    Reject(Vec<GateRejection>),
}

/// finish gate を副作用なしで評価する。
pub fn evaluate(inputs: &GateInputs<'_>) -> GateVerdict {
    let mut rejections = Vec::new();

    if inputs.deliverable_branch.is_none() {
        rejections.push(GateRejection::NoDeliverableBranch);
    }

    let Some(pr) = inputs.pr else {
        rejections.push(GateRejection::NoPullRequest);
        return GateVerdict::Reject(rejections);
    };

    if pr.repo != inputs.expected_repo {
        rejections.push(GateRejection::PullRequestRepoMismatch {
            expected: inputs.expected_repo.to_string(),
            actual: pr.repo.clone(),
        });
    }
    if pr.base_ref != inputs.expected_base {
        rejections.push(GateRejection::PullRequestBaseMismatch {
            expected: inputs.expected_base.to_string(),
            actual: pr.base_ref.clone(),
        });
    }

    let Some(current_head) = inputs.current_head else {
        // 最新 remote head を証明できない限り受理しない (fail-closed, AC3)。
        rejections.push(GateRejection::RemoteHeadUnavailable {
            detail: "remote head could not be fetched".to_string(),
        });
        return GateVerdict::Reject(rejections);
    };
    if pr.head_sha != current_head {
        rejections.push(GateRejection::StaleHead {
            evidence: "pull_request".to_string(),
            evidence_head: pr.head_sha.clone(),
            current_head: current_head.to_string(),
        });
    }

    match inputs.ci {
        Some(ci) if ci.head_sha == current_head => match &ci.state {
            CiState::Pending => rejections.push(GateRejection::CiPending {
                head_sha: current_head.to_string(),
            }),
            CiState::Green => {}
            CiState::Failing { summary } => rejections.push(GateRejection::CiFailing {
                head_sha: current_head.to_string(),
                summary: summary.clone(),
            }),
        },
        Some(_) | None => rejections.push(GateRejection::CiMissing {
            head_sha: current_head.to_string(),
        }),
    }

    match inputs.criteria {
        Some(criteria) if criteria.head_sha == current_head => {
            let ids = criteria
                .checklist
                .iter()
                .filter_map(|check| match check.status {
                    CriterionStatus::Met => None,
                    CriterionStatus::Unmet | CriterionStatus::Unknown => Some(check.id.clone()),
                })
                .collect::<Vec<_>>();
            if !ids.is_empty() {
                rejections.push(GateRejection::CriteriaUnmet {
                    head_sha: current_head.to_string(),
                    ids,
                });
            }
        }
        Some(_) | None => rejections.push(GateRejection::CriteriaUnverified {
            head_sha: current_head.to_string(),
        }),
    }

    match inputs.review {
        Some(review) if review.head_sha != current_head => {
            rejections.push(GateRejection::ReviewStale {
                reviewed_head: review.head_sha.clone(),
                current_head: current_head.to_string(),
            });
        }
        Some(review) => match &review.verdict {
            ReviewVerdict::Approve => {}
            ReviewVerdict::RequestUpdate { .. } => {
                rejections.push(GateRejection::ReviewRequestUpdate {
                    head_sha: current_head.to_string(),
                    round: review.round,
                });
            }
        },
        None => rejections.push(GateRejection::ReviewMissing {
            head_sha: current_head.to_string(),
        }),
    }

    if inputs.review_rounds_used >= inputs.max_review_rounds {
        rejections.push(GateRejection::ReviewRoundsExhausted {
            rounds: inputs.review_rounds_used,
        });
    }

    if !rejections.is_empty() {
        return GateVerdict::Reject(rejections);
    }

    let (Some(ci), Some(criteria), Some(review)) = (inputs.ci, inputs.criteria, inputs.review)
    else {
        return GateVerdict::Reject(rejections);
    };
    GateVerdict::Accept(GateSnapshot {
        repo: pr.repo.clone(),
        pr_number: pr.number,
        base_ref: pr.base_ref.clone(),
        head_sha: current_head.to_string(),
        ci: ci.state.clone(),
        criteria_round: criteria.round,
        review_round: review.round,
        reviewer_run_id: review.reviewer_run_id.clone(),
    })
}
