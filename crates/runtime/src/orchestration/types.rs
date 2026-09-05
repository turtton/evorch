//! オーケストレーションサブシステムが共有するスキーマ型の再輸出と、
//! 承認済みマージバインディングの型定義。

pub use event_bus::{
    GateRejection, GateSnapshot, GoalStage, GoalState, MergeBinding, OrchestratorEvent,
};

/// 承認済みマージバインディング。
///
/// crate 外に constructor を持たず、`MergeApprovals::approve` (T2.2) のみが
/// 構築できるため、[`DeliveryPort::merge_pr`](crate::orchestration::DeliveryPort::merge_pr)
/// は承認を経由しないマージ要求を型で受け付けない。
///
/// ```compile_fail,E0451
/// use event_bus::{CiState, GateSnapshot, MergeBinding};
/// use runtime::orchestration::ApprovedMerge;
///
/// let snapshot = GateSnapshot {
///     repo: "turtton/evorch".into(), pr_number: 101, base_ref: "main".into(),
///     head_sha: "a".repeat(40), ci: CiState::Green, criteria_round: 1,
///     review_round: 1, reviewer_run_id: "review-1".into(),
/// };
/// let binding = MergeBinding {
///     token_id: "token".into(), repo: snapshot.repo.clone(), pr_number: 101,
///     head_sha: snapshot.head_sha.clone(), snapshot,
/// };
/// let _forged = ApprovedMerge { binding };
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovedMerge {
    pub(crate) binding: MergeBinding,
}

impl ApprovedMerge {
    /// 承認済みバインディングを返す。
    ///
    /// T2.2 の `MergeApprovals` が構築し、T2.3 の `ShellDeliveryAdapter` が
    /// `--match-head-commit` 引数の構築に使うまでの仮死状態である。
    pub fn binding(&self) -> &MergeBinding {
        &self.binding
    }
}
