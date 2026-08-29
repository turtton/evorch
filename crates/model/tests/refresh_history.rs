//! `ModelCatalog::refresh` のリフレッシュオーケストレーションと
//! カタログ更新履歴記録の結合テストです。
//!
//! フェッチャーはネットワークに触れないインプロセスのモックで置き換え、
//! キャッシュには一時ディレクトリ、履歴にはメモリ上の `storage::Database`
//! を使います。

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use model::{
    Availability, CatalogCache, CatalogCapabilities, CatalogEntry, CatalogFetcher, CatalogSource,
    ModelCatalog, ModelError, ProviderType, RefreshSource,
};
use storage::Database;

/// 呼び出しの有無を記録し、固定の結果またはエラーを返すフェッチャー。
struct MockFetcher {
    /// `fetch` が返す結果。
    result: Result<Vec<CatalogEntry>, ModelError>,
    /// `fetch` が呼ばれたかどうか。
    called: AtomicBool,
}

impl MockFetcher {
    /// 指定項目を返す成功フェッチャーを生成する。
    fn ok(entries: Vec<CatalogEntry>) -> Self {
        Self {
            result: Ok(entries),
            called: AtomicBool::new(false),
        }
    }

    /// 常にフェッチ失敗を返すフェッチャーを生成する。
    fn fail(message: &str) -> Self {
        Self {
            result: Err(ModelError::Fetch(message.to_string())),
            called: AtomicBool::new(false),
        }
    }

    /// `fetch` が呼ばれたかどうかを返す。
    fn was_called(&self) -> bool {
        self.called.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl CatalogFetcher for MockFetcher {
    async fn fetch(&self) -> Result<Vec<CatalogEntry>, ModelError> {
        self.called.store(true, Ordering::SeqCst);
        self.result.clone()
    }
}

/// テスト用のカタログ項目。
fn test_entry(model_id: &str) -> CatalogEntry {
    CatalogEntry {
        model_id: model_id.to_string(),
        provider: ProviderType::OpenAi,
        context_window: 64_000,
        max_output_tokens: 8_000,
        capabilities: CatalogCapabilities {
            tool_calling: true,
            reasoning: false,
            prompt_cache: false,
        },
        price: None,
        availability: Availability::Available,
        source: CatalogSource::ModelsDev,
        attributes_confirmed: true,
    }
}

// Given: 空のキャッシュと成功するフェッチャー、組み込みカタログ
// When: refresh を実行する
// Then: 取得項目がマージされキャッシュされ、履歴に source="models-dev" の
//       記録が 1 件残る
#[tokio::test]
async fn refresh_success_merges_and_records_history() {
    let dir = tempfile::tempdir().expect("一時ディレクトリを作成できる");
    let cache = CatalogCache::new(dir.path(), Duration::from_secs(3_600));
    let db = Database::open_in_memory().expect("メモリ DB を開ける");
    let mut catalog = ModelCatalog::builtin();
    let fetcher = MockFetcher::ok(vec![test_entry("gemma-3-27b"), test_entry("gpt-4o")]);

    let outcome = catalog
        .refresh(&fetcher, &cache, &db)
        .await
        .expect("refresh は成功する");

    assert_eq!(
        outcome.source,
        RefreshSource::ModelsDev,
        "供給源は models-dev"
    );
    assert_eq!(
        outcome.merged_count, 6,
        "組み込み 5 項目 + 新規 1 項目 = 6 項目"
    );
    let entry = catalog
        .get("gemma-3-27b")
        .expect("取得した新規モデルがマージされる");
    assert_eq!(
        entry.source,
        CatalogSource::ModelsDev,
        "マージ後の供給源は ModelsDev"
    );
    assert!(cache.load().is_some(), "取得結果がキャッシュに保存される");

    let records = db.catalog_updates().expect("履歴を一覧できる");
    assert_eq!(records.len(), 1, "履歴は 1 件");
    assert_eq!(records[0].source, "models-dev");
    assert_eq!(records[0].model_count, 6, "マージ後の項目数が記録される");
}

// Given: キャッシュが存在せず、常に失敗するフェッチャー
// When: refresh を実行する
// Then: 組み込みカタログはそのまま維持され、履歴 source="builtin" の詳細に
//       フェッチエラーの文言が含まれる
#[tokio::test]
async fn fetch_failure_falls_back_to_builtin_and_records_history() {
    let dir = tempfile::tempdir().expect("一時ディレクトリを作成できる");
    let cache = CatalogCache::new(dir.path(), Duration::from_secs(3_600));
    let db = Database::open_in_memory().expect("メモリ DB を開ける");
    let mut catalog = ModelCatalog::builtin();
    let before = catalog.entries().clone();
    let fetcher = MockFetcher::fail("boom: 模擬ネットワーク障害");

    let outcome = catalog
        .refresh(&fetcher, &cache, &db)
        .await
        .expect("refresh は成功する");

    assert_eq!(outcome.source, RefreshSource::Builtin, "供給源は builtin");
    assert_eq!(outcome.merged_count, 5, "組み込みカタログのまま");
    assert_eq!(catalog.entries(), &before, "組み込みカタログは変更されない");

    let records = db.catalog_updates().expect("履歴を一覧できる");
    assert_eq!(records.len(), 1, "履歴は 1 件");
    assert_eq!(records[0].source, "builtin");
    assert_eq!(records[0].model_count, 5, "維持した項目数が記録される");
    assert!(
        records[0].detail.contains("boom"),
        "詳細にフェッチエラーの文言が含まれる: {}",
        records[0].detail
    );
}

// Given: TTL 内のエントリを事前保存したキャッシュと、失敗するフェッチャー
// When: refresh を実行する
// Then: fetch は呼ばれず、キャッシュ項目がマージされ、履歴 source="cache" と
//       なる
#[tokio::test]
async fn fresh_cache_skips_fetch() {
    let dir = tempfile::tempdir().expect("一時ディレクトリを作成できる");
    let cache = CatalogCache::new(dir.path(), Duration::from_secs(3_600));
    let db = Database::open_in_memory().expect("メモリ DB を開ける");
    cache
        .store(&[test_entry("cached-model"), test_entry("gpt-4o")])
        .expect("キャッシュを事前保存できる");
    let mut catalog = ModelCatalog::builtin();
    let fetcher = MockFetcher::fail("boom: 呼ばれてはならない");

    let outcome = catalog
        .refresh(&fetcher, &cache, &db)
        .await
        .expect("refresh は成功する");

    assert!(
        !fetcher.was_called(),
        "TTL 内のキャッシュがあるため fetch は呼ばれない"
    );
    assert_eq!(outcome.source, RefreshSource::Cache, "供給源は cache");
    assert_eq!(
        outcome.merged_count, 6,
        "組み込み 5 項目 + 新規 1 項目 = 6 項目"
    );
    let entry = catalog
        .get("cached-model")
        .expect("キャッシュの項目がマージされる");
    assert_eq!(entry.source, CatalogSource::ModelsDev);

    let records = db.catalog_updates().expect("履歴を一覧できる");
    assert_eq!(records.len(), 1, "履歴は 1 件");
    assert_eq!(records[0].source, "cache");
    assert_eq!(records[0].model_count, 6, "マージ後の項目数が記録される");
}

// Given: 期限切れまで待機したキャッシュと、常に失敗するフェッチャー
// When: refresh を実行する
// Then: fetch の試行後に期限切れキャッシュがフォールバック採用され、
//       履歴 source="cache-stale" の詳細にフェッチエラーの文言が含まれる
#[tokio::test]
async fn stale_cache_used_when_fetch_fails() {
    let dir = tempfile::tempdir().expect("一時ディレクトリを作成できる");
    let cache = CatalogCache::new(dir.path(), Duration::from_millis(1));
    let db = Database::open_in_memory().expect("メモリ DB を開ける");
    cache
        .store(&[test_entry("stale-model")])
        .expect("キャッシュを事前保存できる");
    std::thread::sleep(Duration::from_millis(10));
    assert!(cache.load().is_none(), "事前条件: キャッシュは期限切れ");
    let mut catalog = ModelCatalog::builtin();
    let fetcher = MockFetcher::fail("boom: 模擬ネットワーク障害");

    let outcome = catalog
        .refresh(&fetcher, &cache, &db)
        .await
        .expect("refresh は成功する");

    assert!(fetcher.was_called(), "期限切れのため fetch を試行する");
    assert_eq!(
        outcome.source,
        RefreshSource::CacheStale,
        "供給源は cache-stale"
    );
    assert_eq!(
        outcome.merged_count, 6,
        "組み込み 5 項目 + 新規 1 項目 = 6 項目"
    );
    assert!(
        catalog.get("stale-model").is_some(),
        "期限切れキャッシュの項目がマージされる"
    );

    let records = db.catalog_updates().expect("履歴を一覧できる");
    assert_eq!(records.len(), 1, "履歴は 1 件");
    assert_eq!(records[0].source, "cache-stale");
    assert_eq!(records[0].model_count, 6, "マージ後の項目数が記録される");
    assert!(
        records[0].detail.contains("boom"),
        "詳細にフェッチエラーの文言が含まれる: {}",
        records[0].detail
    );
}
