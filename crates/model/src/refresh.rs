//! モデルカタログのリフレッシュオーケストレーションです。
//!
//! ADR 0013 のハイブリッド供給源のうち外部系 (ディスクキャッシュ・
//! models.dev) と組み込みカタログを優先順に解決する状態機械を提供し、
//! 採用した供給源をカタログ更新履歴 (`storage` の `catalog_updates`) へ
//! 記録します。

use std::time::SystemTime;

use storage::{CatalogUpdateRecord, StorageHandle};

use crate::cache::CatalogCache;
use crate::catalog::ModelCatalog;
use crate::error::ModelError;
use crate::fetch::CatalogFetcher;

/// リフレッシュで採用された供給源。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshSource {
    /// TTL 内のディスクキャッシュ。
    Cache,
    /// 外部カタログ (models.dev) からの取得。
    ModelsDev,
    /// TTL 期限切れだが、取得失敗時のフォールバックとして採用された
    /// ディスクキャッシュ。
    CacheStale,
    /// 外部取得にもキャッシュにも失敗した場合に維持される組み込みカタログ。
    Builtin,
}

impl RefreshSource {
    /// カタログ更新履歴の `source` に保存する文字列表現を返す。
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Cache => "cache",
            Self::ModelsDev => "models-dev",
            Self::CacheStale => "cache-stale",
            Self::Builtin => "builtin",
        }
    }
}

/// カタログリフレッシュの結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefreshOutcome {
    /// 採用された供給源。
    pub source: RefreshSource,
    /// リフレッシュ後 (マージ適用後) のカタログ項目数。
    pub merged_count: usize,
}

impl ModelCatalog {
    /// キャッシュ・外部カタログ・組み込みカタログの優先順でリフレッシュする。
    ///
    /// 状態機械は以下の優先順で解決します:
    ///
    /// 1. TTL 内のキャッシュ ([`CatalogCache::load`]) に有効な項目があれば
    ///    それをマージし、供給源 `cache` として履歴に記録します。
    ///    ネットワーク取得は行いません。
    /// 2. なければ [`CatalogFetcher::fetch`] で外部カタログを取得します。
    ///    成功すればマージしキャッシュへ保存して、供給源 `models-dev` として
    ///    履歴に記録します。キャッシュへの保存はベストエフォートのため、
    ///    保存の失敗はリフレッシュの失敗とはせず警告のみを出します。
    /// 3. 取得に失敗すれば、TTL を無視したキャッシュ
    ///    ([`CatalogCache::load_ignoring_ttl`]) へフォールバックします。
    ///    項目があればマージし、供給源 `cache-stale` として、詳細に取得
    ///    エラーを含めて履歴に記録します。
    /// 4. キャッシュもなければ組み込みカタログをそのまま維持し、供給源
    ///    `builtin` として、詳細に取得エラーを含めて履歴に記録します。
    ///
    /// 履歴の記録は `handle.record_catalog_update` (型付き writer API) のみを
    /// 使い、このクレートは SQL を直接扱いません。`model_count` にはマージ
    /// 後のカタログ項目数を、`recorded_at_ns` には [`SystemTime::now`] の
    /// UNIX epoch ナノ秒を記録します (時刻表現は `storage` の他レコードと
    /// 共通のため、変換には `storage::system_time_to_ns` を使います)。
    ///
    /// # Errors
    ///
    /// 履歴の書き込みに失敗した場合、または項目数・時刻の変換に失敗した
    /// 場合に [`ModelError::History`] を返します。外部カタログ取得自体の
    /// 失敗は手順 3・4 のフォールバックで消化されるため、エラーとして
    /// 返ることはありません。
    pub async fn refresh(
        &mut self,
        fetcher: &dyn CatalogFetcher,
        cache: &CatalogCache,
        handle: &StorageHandle,
    ) -> Result<RefreshOutcome, ModelError> {
        // 1. TTL 内のキャッシュがあればネットワーク取得を行わない。
        if let Some(cached_entries) = cache.load() {
            self.merge_models_dev(cached_entries);
            let detail = "TTL 内のキャッシュを利用しました (外部カタログの取得をスキップしました)"
                .to_string();
            return self.record_refresh(handle, RefreshSource::Cache, detail);
        }

        // 2. 外部カタログを取得する。
        let fetched = match fetcher.fetch().await {
            Ok(fetched) => fetched,
            Err(fetch_error) => {
                // 3. 取得失敗時は TTL を無視したキャッシュへフォールバックする。
                if let Some(stale_entries) = cache.load_ignoring_ttl() {
                    self.merge_models_dev(stale_entries);
                    let detail = format!(
                        "外部カタログの取得に失敗したため期限切れのキャッシュを利用しました: {fetch_error}"
                    );
                    return self.record_refresh(handle, RefreshSource::CacheStale, detail);
                }
                // 4. キャッシュもなければ組み込みカタログを維持する。
                let detail = format!(
                    "外部カタログの取得とキャッシュのいずれも利用できないため組み込みカタログを維持しました: {fetch_error}"
                );
                return self.record_refresh(handle, RefreshSource::Builtin, detail);
            }
        };

        // キャッシュはベストエフォートのため、保存失敗は警告のみとする。
        // マージが `fetched` を消費するため、参照を借りる保存を先に行う。
        if let Err(cache_error) = cache.store(&fetched) {
            tracing::warn!(
                error = %cache_error,
                "取得したカタログのキャッシュ保存に失敗しました"
            );
        }
        self.merge_models_dev(fetched);
        self.record_refresh(
            handle,
            RefreshSource::ModelsDev,
            "外部カタログ (models.dev) の取得に成功しました".to_string(),
        )
    }

    /// マージ後のカタログ状態をカタログ更新履歴へ記録し、結果を組み立てる。
    fn record_refresh(
        &self,
        handle: &StorageHandle,
        source: RefreshSource,
        detail: String,
    ) -> Result<RefreshOutcome, ModelError> {
        let entry_count = self.entries().len();
        let model_count = u32::try_from(entry_count).map_err(|_| {
            ModelError::History(format!(
                "カタログ項目数が u32 の範囲を超えました: {entry_count}"
            ))
        })?;
        let recorded_at_ns = storage::system_time_to_ns(SystemTime::now())
            .map_err(|err| ModelError::History(err.to_string()))?;
        let record = CatalogUpdateRecord {
            source: source.as_str().to_string(),
            model_count,
            detail,
            recorded_at_ns,
        };
        handle
            .record_catalog_update(&record)
            .map_err(|err| ModelError::History(err.to_string()))?;
        Ok(RefreshOutcome {
            source,
            merged_count: entry_count,
        })
    }
}
