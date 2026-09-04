//! goal 配信 (push / PR / CI 観測 / merge / closeout) のポート境界。

use async_trait::async_trait;
use event_bus::{CloseoutStep, GateEvidence};

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

/// demo / headless 用の scripted fixture アダプタ (W0 は skeleton)。
///
/// T2.2 で scripted happy path と呼び出し記録を実装する。それまでの呼び出しは
/// すべて [`DeliveryError::Unsupported`] で失敗する (ADR 0010: 静かに成功しない)。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FixtureDeliveryAdapter;

#[async_trait]
impl DeliveryPort for FixtureDeliveryAdapter {
    async fn push_branch(&self, _branch: &str) -> Result<(), DeliveryError> {
        Err(DeliveryError::Unsupported)
    }

    async fn find_or_create_pr(
        &self,
        _branch: &str,
        _base_ref: &str,
        _title: &str,
        _body: &str,
    ) -> Result<GateEvidence, DeliveryError> {
        Err(DeliveryError::Unsupported)
    }

    async fn pr_status(&self, _repo: &str, _number: u64) -> Result<GateEvidence, DeliveryError> {
        Err(DeliveryError::Unsupported)
    }

    async fn ci_status(&self, _repo: &str, _head_sha: &str) -> Result<GateEvidence, DeliveryError> {
        Err(DeliveryError::Unsupported)
    }

    async fn merge_pr(&self, _approved: &ApprovedMerge) -> Result<String, DeliveryError> {
        Err(DeliveryError::Unsupported)
    }

    async fn closeout_step(
        &self,
        _goal_id: &str,
        _step: CloseoutStep,
    ) -> Result<Option<String>, DeliveryError> {
        Err(DeliveryError::Unsupported)
    }
}
