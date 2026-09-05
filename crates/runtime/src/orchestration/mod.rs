//! オーケストレーションループ (goal supervisor / gate / delivery / review) の
//! 型とポート境界。
//!
//! W0 ではインターフェース契約のみを固定する。振る舞い (GoalLedger /
//! GoalSupervisor / ShellDeliveryAdapter / MergeApprovals) は後続ウェーブで
//! 追加され、本モジュールで宣言したシグネチャを変更してはならない。

use std::future::Future;
use std::pin::Pin;

use crate::run::RunId;

use gate::GateVerdict;

pub mod approval;
pub mod closeout;
pub mod continuation;
pub mod delivery;
pub mod gate;
pub mod ledger;
pub mod prompts;
pub mod registry;
pub mod review;
pub mod shell_delivery;
pub mod stall;
pub mod supervisor;
pub mod types;

pub use delivery::{DeliveryError, DeliveryPort, FixtureDeliveryAdapter};
pub use types::{
    ApprovedMerge, GateRejection, GateSnapshot, GoalStage, GoalState, MergeBinding,
    OrchestratorEvent,
};

/// finish 判定の seam (issue #73 T1.3)。
///
/// T2.1 の `GoalRegistry` が実装し、T3.1 の `meta::finish` が
/// [`AgentRuntime::goal_gate`](crate::runtime::AgentRuntime::goal_gate) 経由で
/// 参照する。返り値の future は hand-written boxed future (async-trait desugar
/// 形) とし、async-trait crate の新規依存を導入しない。
pub trait GoalGate: Send + Sync {
    /// caller run の finish を判定する。
    ///
    /// `None` は「caller run がどの goal にも紐付いていない」ことを意味し、
    /// 呼出し側は legacy の即時受理へフォールバックする。
    fn evaluate_finish<'a>(
        &'a self,
        run: RunId,
    ) -> Pin<Box<dyn Future<Output = Option<GateVerdict>> + Send + 'a>>;
}
