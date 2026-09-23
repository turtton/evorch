//! モデルカタログとルーティング解決で共有するモデル定義のコア型を提供します。
//!
//! models.dev 等の外部カタログの取得 (`fetch`) とディスクキャッシュ
//! (`cache`)、プロバイダ検出結果の統合、外部取得のリフレッシュ
//! (`refresh`) を提供します。

pub mod cache;
pub mod capabilities;
pub mod catalog;
pub mod error;
pub mod fetch;
pub mod refresh;
pub mod types;

pub use cache::CatalogCache;
pub use capabilities::{CapabilitySupport, ModelCapabilities};
pub use catalog::{Capability, ModelCatalog};
pub use error::ModelError;
pub use fetch::{CatalogFetcher, ReqwestModelsDevFetcher};
pub use refresh::{RefreshOutcome, RefreshSource};
pub use types::{
    ApiProtocol, Availability, CatalogCapabilities, CatalogEntry, CatalogSource, LogicalModelId,
    ModelPrice, ProviderType,
};
