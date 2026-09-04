//! オーケストレーションループ (goal supervisor / gate / delivery / review) の
//! 型とポート境界。
//!
//! W0 ではインターフェース契約のみを固定する。振る舞い (GoalLedger /
//! GoalSupervisor / ShellDeliveryAdapter / MergeApprovals) は後続ウェーブで
//! 追加され、本モジュールで宣言したシグネチャを変更してはならない。

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
