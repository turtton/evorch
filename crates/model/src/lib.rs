//! モデルカタログとルーティング解決で共有するモデル定義のコア型を提供します。
//!
//! ADR 0013 のハイブリッド 4 供給源カタログのうち、オフラインで完結する
//! 組み込みカタログと共通型に加え、models.dev 等の外部カタログの取得
//! (`fetch`) とディスクキャッシュ (`cache`)、そしてキャッシュと外部取得を
//! 優先順に解決するリフレッシュ (`refresh`) を提供します。

pub mod cache;
pub mod catalog;
pub mod error;
pub mod fetch;
pub mod refresh;
pub mod types;

pub use cache::CatalogCache;
pub use catalog::{Capability, ModelCatalog};
pub use error::ModelError;
pub use fetch::{CatalogFetcher, ReqwestModelsDevFetcher};
pub use refresh::{RefreshOutcome, RefreshSource};
pub use types::{
    ApiProtocol, Availability, CatalogCapabilities, CatalogEntry, CatalogSource, LogicalModelId,
    ModelPrice, ProviderType,
};
