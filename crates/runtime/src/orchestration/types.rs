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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovedMerge {
    pub(crate) binding: MergeBinding,
}

impl ApprovedMerge {
    /// 承認済みバインディングを返す。
    ///
    /// T2.2 の `MergeApprovals` が構築し、T2.3 の `ShellDeliveryAdapter` が
    /// `--match-head-commit` 引数の構築に使うまでの仮死状態である。
    #[expect(
        dead_code,
        reason = "constructed by MergeApprovals (T2.2) and read by ShellDeliveryAdapter (T2.3)"
    )]
    pub(crate) fn binding(&self) -> &MergeBinding {
        &self.binding
    }
}
