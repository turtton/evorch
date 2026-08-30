//! ロール実行ランタイム (role-execution runtime)。
//!
//! ADR 0002 の capability boundary を持つロール (Orchestrator / Explorer / Worker /
//! Reviewer) を実行する基盤である。
//!
//! - **event-sourced 状態遷移**: AgentRun の位相は
//!   [`LifecycleEvent::AgentRunStateChanged`](event_bus::LifecycleEvent) のみを
//!   通じて変化する。[`state::RunState::transition`] が生成した遷移イベントは
//!   `event-bus` の [`EventBus`](event_bus::EventBus) へ emit される。
//! - **independent contexts**: 各 run は [`context::AgentContext`] を専有所有し、
//!   複数 AgentRun の文脈独立性は所有権によって構成上保証される。
//! - **capability 強制**: [`policy::ExecutionPolicy`] が ADR 0002 の境界を
//!   ツール認可とモデルへのツール公開の 2 点で強制する。
//! - **ルーティングの委譲**: role → model の解決・フォールバックは
//!   v01-routing-profiles が [`model::AgentModel`] 境界の実装として提供する。
//!   runtime は model 名を一切持たない。

mod agent_loop;
pub mod context;
pub mod error;
mod meta;
pub mod model;
pub mod network;
pub mod policy;
pub mod run;
pub mod runtime;
pub mod state;

pub use context::AgentContext;
pub use error::RuntimeError;
pub use event_bus::AgentRunPhase;
pub use model::AgentModel;
pub use network::{SandboxNetworkMode, build_sandbox, sandbox_network_mode};
pub use policy::{ExecutionPolicy, META_OPS, is_meta_op};
pub use run::{AgentInspection, AgentSummary, RunConfig, RunId};
pub use runtime::AgentRuntime;
pub use state::{RunState, is_valid_transition};
