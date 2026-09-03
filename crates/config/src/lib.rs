//! 設定ファイルの型定義とエラー型を提供するリーフクレートです。
//!
//! このクレートは設定スキーマ (バージョン 2) を型として表現します。
//! ファイル読み込み・マージ・マイグレーションは後続タスクで別モジュールとして
//! 追加されます (ADR 0014)。model クレートに依存しないリーフ構造を維持するため、
//! 列挙型はこのクレート内で独自に定義します (ADR 0004)。

mod env;
pub mod error;
pub mod load;
mod merge;
mod migrate;
pub mod presets;
pub mod prompt_sources;
mod schema;
mod strict;
pub mod types;

pub use error::ConfigError;
pub use load::{LoadOptions, user_config_dir};
pub use presets::PresetStore;
pub use prompt_sources::{AgentPromptSources, resolve_prompt_sources};
pub use schema::json_schema;
pub use types::{
    AgentsConfig, ApiProtocolConfig, CURRENT_VERSION, CategoryBindingConfig, Config,
    CredentialRefConfig, DiagnosticsConfig, GenerationOverridesConfig, MetricsConfig, PanelConfig,
    PermissionConfig, ProviderProfileConfig, ProviderTypeConfig, ReasoningEffortConfig,
    ResolvedAgentBinding, RoleBindingConfig, RouteCandidateConfig, RoutingConfig, RulesConfig,
};
