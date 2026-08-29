//! models.dev カタログ取得結果のディスクキャッシュです。
//!
//! ADR 0013 の外部カタログ供給源の取得コストを抑えるため、取得結果を
//! ファイルへ保存し TTL 内は再利用します。

use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use crate::error::ModelError;
use crate::types::CatalogEntry;

/// キャッシュファイル名。
const CACHE_FILE_NAME: &str = "models-dev.json";

/// models.dev カタログ取得結果のディスクキャッシュ。
///
/// `dir` 直下の `models-dev.json` に [`CatalogEntry`] の JSON 配列として
/// 保存します。鮮度はキャッシュファイルの **mtime** で判定し、mtime から
/// `ttl` 以上経過したキャッシュは期限切れとして扱います。
///
/// キャッシュはベストエフォートです。読み込みのあらゆる問題
/// (ファイル不在・mtime 取得失敗・JSON 破損) はエラーではなく `None` として
/// 扱い、呼び出し側は外部カタログの再取得へフォールバックします。
#[derive(Debug, Clone)]
pub struct CatalogCache {
    /// キャッシュファイルを置くディレクトリ。
    dir: PathBuf,
    /// キャッシュの有効期間。
    ttl: Duration,
}

impl CatalogCache {
    /// キャッシュディレクトリと TTL を指定してキャッシュを生成する。
    pub fn new(dir: impl Into<PathBuf>, ttl: Duration) -> Self {
        Self {
            dir: dir.into(),
            ttl,
        }
    }

    /// カタログ項目の列をキャッシュファイルへ保存する。
    ///
    /// キャッシュディレクトリが存在しない場合は作成します。
    ///
    /// # Errors
    /// ディレクトリの作成・項目の直列化・ファイルへの書き込みに失敗した
    /// 場合 [`ModelError::Cache`] を返します。
    pub fn store(&self, entries: &[CatalogEntry]) -> Result<(), ModelError> {
        fs::create_dir_all(&self.dir).map_err(|err| {
            ModelError::Cache(format!("キャッシュディレクトリの作成に失敗しました: {err}"))
        })?;
        let body = serde_json::to_string_pretty(entries)
            .map_err(|err| ModelError::Cache(format!("キャッシュの直列化に失敗しました: {err}")))?;
        fs::write(self.cache_file(), body).map_err(|err| {
            ModelError::Cache(format!("キャッシュファイルの書き込みに失敗しました: {err}"))
        })
    }

    /// キャッシュからカタログ項目の列を読み込む。
    ///
    /// 鮮度はファイルの mtime で判定します。以下の場合は `None` を返します
    /// (いずれもエラーにはしません):
    ///
    /// - キャッシュファイルが存在しない
    /// - ファイルの mtime から `ttl` 以上が経過している (期限切れ)
    /// - ファイル内容が JSON として解析できない
    pub fn load(&self) -> Option<Vec<CatalogEntry>> {
        let path = self.cache_file();
        let metadata = fs::metadata(&path).ok()?;
        let modified = metadata.modified().ok()?;
        // mtime が時計ずれで未来になる場合は年齢 0 として扱う。
        let age = modified.elapsed().unwrap_or(Duration::ZERO);
        if age >= self.ttl {
            return None;
        }
        let body = fs::read_to_string(&path).ok()?;
        serde_json::from_str(&body).ok()
    }

    /// キャッシュファイルのパスを返す。
    fn cache_file(&self) -> PathBuf {
        self.dir.join(CACHE_FILE_NAME)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{
        Availability, CatalogCapabilities, CatalogSource, ModelPrice, ProviderType,
    };

    /// 往復確認用のサンプル項目 2 件。
    fn sample_entries() -> Vec<CatalogEntry> {
        fn entry(
            model_id: &str,
            provider: ProviderType,
            reasoning: bool,
            price: Option<ModelPrice>,
        ) -> CatalogEntry {
            CatalogEntry {
                model_id: model_id.to_string(),
                provider,
                context_window: 200_000,
                max_output_tokens: 64_000,
                capabilities: CatalogCapabilities {
                    tool_calling: true,
                    reasoning,
                    prompt_cache: true,
                },
                price,
                availability: Availability::Available,
                source: CatalogSource::ModelsDev,
                attributes_confirmed: true,
            }
        }

        vec![
            entry(
                "claude-sonnet-4-5",
                ProviderType::Anthropic,
                true,
                Some(ModelPrice {
                    input_per_million_usd: 3.0,
                    output_per_million_usd: 15.0,
                }),
            ),
            entry("gpt-4o", ProviderType::OpenAi, false, None),
        ]
    }

    // Given: 空の一時ディレクトリと十分に長い TTL のキャッシュ
    // When: サンプル項目を store して load する
    // Then: 同一の項目配列が往復し models-dev.json が作成される
    #[test]
    fn cache_store_then_load_roundtrip() {
        let dir = tempfile::tempdir().expect("一時ディレクトリを作成できる");
        let cache = CatalogCache::new(dir.path(), Duration::from_secs(3_600));
        let entries = sample_entries();

        cache.store(&entries).expect("store は成功する");

        assert!(
            dir.path().join("models-dev.json").exists(),
            "所定のファイル名でキャッシュされる"
        );
        let loaded = cache.load().expect("TTL 内のキャッシュは読み込める");
        assert_eq!(loaded, entries, "保存した項目がそのまま読み込める");
    }

    // Given: TTL 1 ミリ秒のキャッシュに保存済みの項目
    // When: TTL を超過するまで待機して load する
    // Then: 期限切れとして None を返す
    #[test]
    fn cache_expired_ttl_returns_none() {
        let dir = tempfile::tempdir().expect("一時ディレクトリを作成できる");
        let cache = CatalogCache::new(dir.path(), Duration::from_millis(1));

        cache.store(&sample_entries()).expect("store は成功する");
        std::thread::sleep(Duration::from_millis(10));

        assert!(cache.load().is_none(), "TTL 超過後の load は None");
    }

    // Given: キャッシュファイル位置に不正な JSON が存在する
    // When: load する
    // Then: 解析失敗として None を返す
    #[test]
    fn cache_corrupt_json_returns_none() {
        let dir = tempfile::tempdir().expect("一時ディレクトリを作成できる");
        let cache = CatalogCache::new(dir.path(), Duration::from_secs(3_600));
        std::fs::write(dir.path().join("models-dev.json"), "corrupt json")
            .expect("不正なキャッシュを書き込ける");

        assert!(cache.load().is_none(), "破損キャッシュの load は None");
    }

    // Given: キャッシュファイルが存在しない
    // When: load する
    // Then: None を返す
    #[test]
    fn cache_missing_file_returns_none() {
        let dir = tempfile::tempdir().expect("一時ディレクトリを作成できる");
        let cache = CatalogCache::new(dir.path(), Duration::from_secs(3_600));

        assert!(cache.load().is_none(), "ファイル不在時の load は None");
    }
}
