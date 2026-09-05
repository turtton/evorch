//! goal 配信 (push / PR / CI 観測 / merge / closeout) のポート境界。

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};

use async_trait::async_trait;
use event_bus::{CiState, CloseoutStep, GateEvidence};

use super::types::ApprovedMerge;

/// 配信操作の失敗。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DeliveryError {
    /// このアダプタが対応しない操作 (headless / fixture 経路など)。
    #[error("delivery operation is not supported")]
    Unsupported,
    /// 配信コマンドが失敗した (詳細は diagnostic から secret を除いて格納する)。
    #[error("delivery command failed: {0}")]
    Command(String),
    /// リモート応答が契約したプロトコル形状に反した。
    #[error("delivery protocol violation: {0}")]
    Protocol(String),
}

/// goal 配信ポート。
///
/// supervisor から見た配信実行 (git push / gh / intent-cli worker) を閉じる
/// 境界で、実装は本番の `ShellDeliveryAdapter` (T2.3) と demo / headless 用の
/// [`FixtureDeliveryAdapter`] (T2.2) の 2 系統。
#[async_trait]
pub trait DeliveryPort: Send + Sync {
    /// デリバラブルブランチをリモートへ push する。
    async fn push_branch(&self, branch: &str) -> Result<(), DeliveryError>;

    /// 既存 PR を検索し、存在しなければ作成する。
    ///
    /// 成功時は [`GateEvidence::PullRequest`] 形状で返す。
    async fn find_or_create_pr(
        &self,
        branch: &str,
        base_ref: &str,
        title: &str,
        body: &str,
    ) -> Result<GateEvidence, DeliveryError>;

    /// PR の現在状態を取得する ([`GateEvidence::PullRequest`])。
    async fn pr_status(&self, repo: &str, number: u64) -> Result<GateEvidence, DeliveryError>;

    /// 指定 head SHA の CI 状態を取得する ([`GateEvidence::Ci`])。
    async fn ci_status(&self, repo: &str, head_sha: &str) -> Result<GateEvidence, DeliveryError>;

    /// 承認済みバインディングで squash merge する。
    ///
    /// 型レベル契約: [`ApprovedMerge`] は crate 外で構築できないため、
    /// 承認を経由しないマージ要求は表現できない。成功時は記録用の詳細を返す。
    async fn merge_pr(&self, approved: &ApprovedMerge) -> Result<String, DeliveryError>;

    /// closeout ステップ (`intent-cli worker ...`) を実行する。
    ///
    /// 成功時は記録された artifact 参照 (あれば) を返す。
    async fn closeout_step(
        &self,
        goal_id: &str,
        step: CloseoutStep,
    ) -> Result<Option<String>, DeliveryError>;
}

/// fixture adapter が観測した DeliveryPort 呼び出し。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeliveryCall {
    /// branch push。
    PushBranch {
        /// push 対象 branch。
        branch: String,
    },
    /// PR 検索または作成。
    FindOrCreatePr {
        /// PR head branch。
        branch: String,
        /// PR base branch。
        base_ref: String,
        /// PR title。
        title: String,
        /// PR body。
        body: String,
    },
    /// PR 状態取得。
    PrStatus {
        /// repository。
        repo: String,
        /// PR number。
        number: u64,
    },
    /// CI 状態取得。
    CiStatus {
        /// repository。
        repo: String,
        /// head SHA。
        head_sha: String,
    },
    /// 承認済み PR merge。
    MergePr {
        /// merge binding。
        binding: event_bus::MergeBinding,
    },
    /// closeout step。
    CloseoutStep {
        /// goal ID。
        goal_id: String,
        /// closeout operation。
        step: CloseoutStep,
    },
}

#[derive(Debug, Default)]
struct FixtureScript {
    push_branch: VecDeque<Result<(), DeliveryError>>,
    find_or_create_pr: VecDeque<Result<GateEvidence, DeliveryError>>,
    pr_status: VecDeque<Result<GateEvidence, DeliveryError>>,
    ci_status: VecDeque<Result<GateEvidence, DeliveryError>>,
    merge_pr: VecDeque<Result<String, DeliveryError>>,
    closeout_step: VecDeque<Result<Option<String>, DeliveryError>>,
    recorded: Vec<DeliveryCall>,
}

/// demo / headless 用の method-scoped scripted fixture adapter。
#[derive(Debug, Clone, Default)]
pub struct FixtureDeliveryAdapter {
    script: Arc<Mutex<FixtureScript>>,
}

impl FixtureDeliveryAdapter {
    /// v0.2 demo の request-update → repair → approve 経路を返す。
    pub fn scripted_happy_path() -> Self {
        const HEAD_A: &str = "a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1";
        const HEAD_B: &str = "a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2";
        let adapter = Self::default();
        adapter.script_push(Ok(()));
        adapter.script_push(Ok(()));
        adapter.script_find_or_create_pr(Ok(pr_evidence(HEAD_A)));
        adapter.script_pr_status(Ok(pr_evidence(HEAD_B)));
        adapter.script_pr_status(Ok(pr_evidence(HEAD_B)));
        adapter.script_pr_status(Ok(pr_evidence(HEAD_B)));
        adapter.script_ci(Ok(ci_evidence(HEAD_A)));
        adapter.script_ci(Ok(ci_evidence(HEAD_B)));
        adapter.script_ci(Ok(ci_evidence(HEAD_B)));
        adapter.script_merge(Ok("merged PR #101".to_string()));
        adapter.script_closeout(Ok(Some("claim:goal".to_string())));
        adapter.script_closeout(Ok(Some("summary:goal".to_string())));
        adapter.script_closeout(Ok(None));
        adapter
    }

    /// push result を末尾へ追加する。
    pub fn script_push(&self, result: Result<(), DeliveryError>) {
        lock(&self.script).push_branch.push_back(result);
    }

    /// PR 作成/検索 result を末尾へ追加する。
    pub fn script_find_or_create_pr(&self, result: Result<GateEvidence, DeliveryError>) {
        lock(&self.script).find_or_create_pr.push_back(result);
    }

    /// PR status result を末尾へ追加する。
    pub fn script_pr_status(&self, result: Result<GateEvidence, DeliveryError>) {
        lock(&self.script).pr_status.push_back(result);
    }

    /// CI status result を末尾へ追加する。
    pub fn script_ci(&self, result: Result<GateEvidence, DeliveryError>) {
        lock(&self.script).ci_status.push_back(result);
    }

    /// merge result を末尾へ追加する。
    pub fn script_merge(&self, result: Result<String, DeliveryError>) {
        lock(&self.script).merge_pr.push_back(result);
    }

    /// closeout result を末尾へ追加する。
    pub fn script_closeout(&self, result: Result<Option<String>, DeliveryError>) {
        lock(&self.script).closeout_step.push_back(result);
    }

    /// 記録済み呼び出しの snapshot を返す。
    pub fn recorded(&self) -> Vec<DeliveryCall> {
        lock(&self.script).recorded.clone()
    }
}

#[async_trait]
impl DeliveryPort for FixtureDeliveryAdapter {
    async fn push_branch(&self, branch: &str) -> Result<(), DeliveryError> {
        let mut script = lock(&self.script);
        script.recorded.push(DeliveryCall::PushBranch {
            branch: branch.to_string(),
        });
        script
            .push_branch
            .pop_front()
            .unwrap_or(Err(DeliveryError::Unsupported))
    }

    async fn find_or_create_pr(
        &self,
        branch: &str,
        base_ref: &str,
        title: &str,
        body: &str,
    ) -> Result<GateEvidence, DeliveryError> {
        let mut script = lock(&self.script);
        script.recorded.push(DeliveryCall::FindOrCreatePr {
            branch: branch.to_string(),
            base_ref: base_ref.to_string(),
            title: title.to_string(),
            body: body.to_string(),
        });
        script
            .find_or_create_pr
            .pop_front()
            .unwrap_or(Err(DeliveryError::Unsupported))
    }

    async fn pr_status(&self, repo: &str, number: u64) -> Result<GateEvidence, DeliveryError> {
        let mut script = lock(&self.script);
        script.recorded.push(DeliveryCall::PrStatus {
            repo: repo.to_string(),
            number,
        });
        script
            .pr_status
            .pop_front()
            .unwrap_or(Err(DeliveryError::Unsupported))
    }

    async fn ci_status(&self, repo: &str, head_sha: &str) -> Result<GateEvidence, DeliveryError> {
        let mut script = lock(&self.script);
        script.recorded.push(DeliveryCall::CiStatus {
            repo: repo.to_string(),
            head_sha: head_sha.to_string(),
        });
        script
            .ci_status
            .pop_front()
            .unwrap_or(Err(DeliveryError::Unsupported))
    }

    async fn merge_pr(&self, approved: &ApprovedMerge) -> Result<String, DeliveryError> {
        let mut script = lock(&self.script);
        script.recorded.push(DeliveryCall::MergePr {
            binding: approved.binding().clone(),
        });
        script
            .merge_pr
            .pop_front()
            .unwrap_or(Err(DeliveryError::Unsupported))
    }

    async fn closeout_step(
        &self,
        goal_id: &str,
        step: CloseoutStep,
    ) -> Result<Option<String>, DeliveryError> {
        let mut script = lock(&self.script);
        script.recorded.push(DeliveryCall::CloseoutStep {
            goal_id: goal_id.to_string(),
            step,
        });
        script
            .closeout_step
            .pop_front()
            .unwrap_or(Err(DeliveryError::Unsupported))
    }
}

fn lock(script: &Mutex<FixtureScript>) -> MutexGuard<'_, FixtureScript> {
    match script.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn pr_evidence(head_sha: &str) -> GateEvidence {
    GateEvidence::PullRequest {
        repo: "turtton/evorch".to_string(),
        number: 101,
        url: "https://github.com/turtton/evorch/pull/101".to_string(),
        base_ref: "main".to_string(),
        head_sha: head_sha.to_string(),
    }
}

fn ci_evidence(head_sha: &str) -> GateEvidence {
    GateEvidence::Ci {
        head_sha: head_sha.to_string(),
        state: CiState::Green,
    }
}
