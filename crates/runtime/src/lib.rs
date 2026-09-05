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
//! - **ルーティング境界**: kernel の agent loop は [`model::AgentModel`] だけを参照し、
//!   role → model 解決を行わない。edge の [`compose`] が設定・routing・provider を接続する。

mod agent_loop;
pub(crate) mod compaction;
pub mod compose;
pub mod context;
pub mod entry_routing;
pub mod error;
pub mod escalation;
pub mod mailbox;
mod meta;
pub mod model;
pub mod network;
pub mod orchestration;
pub mod policy;
pub mod prompt;
pub mod rules;
pub mod run;
pub mod runtime;
pub mod skill;
pub mod state;
pub mod workspace;

pub use compose::{
    ComposedRuntime, CompositionError, ModelIdentity, ModelSource, RoutedModel, RuntimeComposition,
    compose_runtime,
};
pub use context::{AgentContext, CompactionCheckpoint};
pub use entry_routing::{
    COORDINATION_KEYWORDS, DIRECT_KEYWORDS, EntryRouter, LocalVerdict, RoutingDecision,
    UncertainReason, classify_local,
};
pub use error::RuntimeError;
pub use escalation::{EscalationMemo, EscalationSettings};
pub use event_bus::{AgentRunPhase, RoutingSource};
// Role は delegate API の引数型として既に露出しており、呼出側が agents crate 直接依存なしに使えるようにする。
pub use agents::Role;
pub use mailbox::{MAILBOX_CAPACITY, RunMailbox};
pub use model::{AgentInvocationContext, AgentModel};
// オーケストレーション契約型は後続ウェーブ (T1.2–T3.1) がこの経路で参照する。
pub use network::{
    NetworkAccessDecision, SandboxNetworkMode, build_sandbox, judge_web_network_access,
    sandbox_network_mode,
};
pub use orchestration::{
    ApprovedMerge, DeliveryError, DeliveryPort, FixtureDeliveryAdapter, GateRejection,
    GateSnapshot, GoalLedger, GoalSnapshot, GoalSpec, GoalStage, GoalState, GoalSupervisor,
    MergeApprovals, MergeBinding, OrchestrationSettings, OrchestratorEvent, ShellDeliveryAdapter,
    SupervisorHandle,
};
pub use policy::{ExecutionPolicy, META_OPS, is_meta_op};
pub use prompt::{
    AvailableAgent, AvailableSkill, CatalogBuildInput, ModelFamily, PromptCompositionError,
    SystemPromptCatalog, SystemPromptCatalogError, TriggerSource, build_catalog,
};
pub use rules::{ProjectTrust, RulesSession, RulesSettings, RulesSource};
pub use run::{
    AgentInspection, AgentSummary, MergeMode, RunConfig, RunId, WorkspaceInspection, WorkspaceMode,
};
pub use runtime::{AgentRuntime, IsolatedMounts, SandboxFactory, production_executor};
pub use skill::{
    SkillDiagnostic, SkillEntry, SkillFrontmatter, SkillLoadError, SkillRegistry,
    SkillResourceError, SkillScope, SkillValidationError, default_skill_dirs, discover_skills,
    parse_and_validate, read_skill_resource, split_frontmatter,
};
pub use state::{RunState, is_valid_transition};
