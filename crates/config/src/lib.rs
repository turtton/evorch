//! 設定ファイルの型定義とエラー型を提供するリーフクレートです。
//!
//! このクレートは設定スキーマ (バージョン 2) を型として表現します。
//! ファイル読み込み・マージ・マイグレーションは後続タスクで別モジュールとして
//! 追加されます (ADR 0014)。model クレートに依存しないリーフ構造を維持するため、
//! 列挙型はこのクレート内で独自に定義します (ADR 0004)。

pub mod error;
pub mod types;

pub use error::ConfigError;
pub use types::{
    ApiProtocolConfig, CURRENT_VERSION, Config, CredentialRefConfig, DiagnosticsConfig,
    MetricsConfig, PanelConfig, PermissionConfig, ProviderProfileConfig, ProviderTypeConfig,
    RouteCandidateConfig, RoutingConfig,
};
