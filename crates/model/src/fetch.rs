//! models.dev 等の外部カタログを HTTP 経由で取得する抽象とその実装です。
//!
//! ADR 0013 の「外部カタログ」供給源を担います。取得結果は
//! [`crate::catalog::ModelCatalog::merge_models_dev`] でカタログへマージされます。

use async_trait::async_trait;
use serde::Deserialize;

use crate::error::ModelError;
use crate::types::{
    Availability, CatalogCapabilities, CatalogEntry, CatalogSource, ModelPrice, ProviderType,
};

/// 外部カタログ取得クライアントの抽象。
///
/// 実装は `Box<dyn CatalogFetcher>` として扱えるよう dyn 互換でなければ
/// なりません (コンパイル時検証は本モジュール末尾の定数アサーションが担います)。
#[async_trait]
pub trait CatalogFetcher: Send + Sync {
    /// 外部カタログを取得してカタログ項目の列を返す。
    ///
    /// # Errors
    /// リクエスト送信や HTTP ステータス失敗は [`ModelError::Fetch`]、
    /// 応答 JSON の解析失敗は [`ModelError::Parse`] を返します。
    /// 未知のプロバイダ slug は警告付きでスキップされ、エラーにはなりません。
    async fn fetch(&self) -> Result<Vec<CatalogEntry>, ModelError>;
}

/// models.dev 形式のカタログドキュメントを HTTP で取得する [`CatalogFetcher`] 実装。
///
/// `GET {base_url}` に以下の形状の JSON ドキュメントを期待します
/// (`<provider-slug>` はプロバイダ種別のケバブケース slug、`<model-id>` は
/// プロバイダ上の実モデル ID です):
///
/// ```json
/// {
///   "<provider-slug>": {
///     "models": {
///       "<model-id>": {
///         "context_window": 200000,
///         "max_output_tokens": 64000,
///         "tool_calling": true,
///         "reasoning": true,
///         "prompt_cache": true,
///         "input_per_million_usd": 3.0,
///         "output_per_million_usd": 15.0
///       }
///     }
///   }
/// }
/// ```
///
/// - トップレベルはプロバイダ slug をキーとしたオブジェクトで、slug は
///   [`crate::types::ProviderType`] の serde 表現 (`"anthropic"`・`"openai"` 等)
///   として解釈します。未知の slug は `tracing::warn` 付きでスキップします。
/// - 各プロバイダの値は `"models"` オブジェクトを持ち、そのキーがモデル ID です。
///   `"models"` を持たない (または `null` の) プロバイダはモデル 0 件として
///   扱います。
/// - 各モデルの `"context_window"`・`"max_output_tokens"`・`"tool_calling"`・
///   `"reasoning"`・`"prompt_cache"` は必須です。
/// - `"input_per_million_usd"`・`"output_per_million_usd"` は省略可能で、
///   両方存在するときのみ [`crate::types::ModelPrice`] を構成します
///   (片方だけの場合は `None`)。
/// - 生成される [`CatalogEntry`] の `availability` は `Available`、`source` は
///   [`crate::types::CatalogSource::ModelsDev`]、`attributes_confirmed` は `true`
///   です (マージ時に再補正されますが、この段階でも正しく設定します)。
#[derive(Debug, Clone)]
pub struct ReqwestModelsDevFetcher {
    /// カタログドキュメントの取得先 URL。
    base_url: String,
    /// 取得に使う HTTP クライアント。
    client: reqwest::Client,
}

impl ReqwestModelsDevFetcher {
    /// 取得先 URL を指定してフェッチャを生成する。
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            client: reqwest::Client::new(),
        }
    }
}

/// models.dev ドキュメントの 1 モデル分の形状。
#[derive(Debug, Deserialize)]
struct ModelsDevModelDoc {
    /// コンテキストウィンドウサイズ (トークン)。
    context_window: u64,
    /// 最大出力トークン数。
    max_output_tokens: u64,
    /// ツール呼び出し (function calling) 対応。
    tool_calling: bool,
    /// 推論 (拡張思考) 対応。
    reasoning: bool,
    /// プロンプトキャッシュ対応。
    prompt_cache: bool,
    /// 入力 1M トークンあたり価格 (USD)。省略可。
    input_per_million_usd: Option<f64>,
    /// 出力 1M トークンあたり価格 (USD)。省略可。
    output_per_million_usd: Option<f64>,
}

#[async_trait]
impl CatalogFetcher for ReqwestModelsDevFetcher {
    async fn fetch(&self) -> Result<Vec<CatalogEntry>, ModelError> {
        let response = self
            .client
            .get(&self.base_url)
            .send()
            .await
            .map_err(|err| ModelError::Fetch(err.to_string()))?;
        let status = response.status();
        if !status.is_success() {
            return Err(ModelError::Fetch(format!(
                "HTTP {status}: カタログの取得に失敗しました"
            )));
        }
        let body = response
            .text()
            .await
            .map_err(|err| ModelError::Fetch(err.to_string()))?;
        let document: serde_json::Value =
            serde_json::from_str(&body).map_err(|err| ModelError::Parse(err.to_string()))?;
        parse_models_dev_document(&document)
    }
}

/// models.dev 形式の JSON ドキュメントをカタログ項目の列へ変換する。
///
/// 未知のプロバイダ slug は警告付きでスキップします。形状違反 (トップレベル・
/// プロバイダ値・`models` がオブジェクトでない、必須フィールドの欠落や型違い)
/// は [`ModelError::Parse`] に変換します。
fn parse_models_dev_document(
    document: &serde_json::Value,
) -> Result<Vec<CatalogEntry>, ModelError> {
    let root = document.as_object().ok_or_else(|| {
        ModelError::Parse("トップレベルが JSON オブジェクトではありません".to_string())
    })?;
    let mut entries = Vec::new();
    for (slug, provider_value) in root {
        let slug_value = serde_json::Value::String(slug.clone());
        let provider = match serde_json::from_value::<ProviderType>(slug_value) {
            Ok(provider) => provider,
            Err(err) => {
                tracing::warn!(
                    provider = %slug,
                    error = %err,
                    "未知のプロバイダ slug をスキップします"
                );
                continue;
            }
        };
        let provider_object = provider_value.as_object().ok_or_else(|| {
            ModelError::Parse(format!(
                "プロバイダ `{slug}` の値がオブジェクトではありません"
            ))
        })?;
        let Some(models) = provider_object
            .get("models")
            .filter(|value| !value.is_null())
        else {
            // `models` を持たないプロバイダはモデル 0 件として扱う。
            continue;
        };
        let model_objects = models.as_object().ok_or_else(|| {
            ModelError::Parse(format!(
                "プロバイダ `{slug}` の `models` がオブジェクトではありません"
            ))
        })?;
        for (model_id, model_value) in model_objects {
            let doc = ModelsDevModelDoc::deserialize(model_value).map_err(|err| {
                ModelError::Parse(format!("モデル `{model_id}` の解析に失敗しました: {err}"))
            })?;
            entries.push(catalog_entry_from_doc(model_id, provider, doc));
        }
    }
    Ok(entries)
}

/// 1 モデル分のドキュメントから [`CatalogEntry`] を構成する。
///
/// 前提として `availability` は `Available`・`source` は
/// [`CatalogSource::ModelsDev`]・`attributes_confirmed` は `true` で構成します
/// (マージ時に再補正されます)。
fn catalog_entry_from_doc(
    model_id: &str,
    provider: ProviderType,
    doc: ModelsDevModelDoc,
) -> CatalogEntry {
    let price = doc
        .input_per_million_usd
        .zip(doc.output_per_million_usd)
        .map(
            |(input_per_million_usd, output_per_million_usd)| ModelPrice {
                input_per_million_usd,
                output_per_million_usd,
            },
        );
    CatalogEntry {
        model_id: model_id.to_string(),
        provider,
        context_window: doc.context_window,
        max_output_tokens: doc.max_output_tokens,
        capabilities: CatalogCapabilities {
            tool_calling: doc.tool_calling,
            reasoning: doc.reasoning,
            prompt_cache: doc.prompt_cache,
        },
        price,
        availability: Availability::Available,
        source: CatalogSource::ModelsDev,
        attributes_confirmed: true,
    }
}

// dyn 互換性 (object safety) のコンパイル時検証。
// CatalogFetcher が dyn 互換でなくなった場合、`dyn CatalogFetcher` 型の
// 構築自体がコンパイルエラーとなる。
const _: () = {
    fn assert_dyn_compatible(_: &dyn CatalogFetcher) {}
    let _ = assert_dyn_compatible as fn(&dyn CatalogFetcher);
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{
        Availability, CatalogCapabilities, CatalogSource, ModelPrice, ProviderType,
    };
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// 期待値のカタログ項目を組み立てる。
    #[allow(clippy::type_complexity)]
    fn expected_entry(
        model_id: &str,
        provider: ProviderType,
        context_window: u64,
        max_output_tokens: u64,
        capabilities: (bool, bool, bool),
        price: Option<(f64, f64)>,
    ) -> CatalogEntry {
        CatalogEntry {
            model_id: model_id.to_string(),
            provider,
            context_window,
            max_output_tokens,
            capabilities: CatalogCapabilities {
                tool_calling: capabilities.0,
                reasoning: capabilities.1,
                prompt_cache: capabilities.2,
            },
            price: price.map(
                |(input_per_million_usd, output_per_million_usd)| ModelPrice {
                    input_per_million_usd,
                    output_per_million_usd,
                },
            ),
            availability: Availability::Available,
            source: CatalogSource::ModelsDev,
            attributes_confirmed: true,
        }
    }

    // Given: 既知 2 プロバイダと未知 slug のプロバイダを含む models.dev ドキュメント
    // When: fetch を実行する
    // Then: 既知プロバイダの 3 項目だけが得られ、部分価格のモデルは価格 None になる
    #[tokio::test(flavor = "multi_thread")]
    async fn models_dev_fetch_happy_path() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "anthropic": {
                    "models": {
                        "claude-sonnet-4-5": {
                            "context_window": 200_000,
                            "max_output_tokens": 64_000,
                            "tool_calling": true,
                            "reasoning": true,
                            "prompt_cache": true,
                            "input_per_million_usd": 3.0,
                            "output_per_million_usd": 15.0
                        },
                        "claude-haiku-4-5": {
                            "context_window": 200_000,
                            "max_output_tokens": 64_000,
                            "tool_calling": true,
                            "reasoning": false,
                            "prompt_cache": true
                        }
                    }
                },
                "openai": {
                    "models": {
                        "gpt-4o": {
                            "context_window": 128_000,
                            "max_output_tokens": 16_384,
                            "tool_calling": true,
                            "reasoning": false,
                            "prompt_cache": true,
                            "input_per_million_usd": 2.5
                        }
                    }
                },
                "unknown-provider": {
                    "models": {
                        "mystery-model": {
                            "context_window": 1,
                            "max_output_tokens": 1,
                            "tool_calling": false,
                            "reasoning": false,
                            "prompt_cache": false
                        }
                    }
                }
            })))
            .mount(&server)
            .await;
        // dyn CatalogFetcher として取得を実行する。
        let fetcher: Box<dyn CatalogFetcher> = Box::new(ReqwestModelsDevFetcher::new(server.uri()));

        let mut entries = fetcher.fetch().await.expect("fetch は成功する");

        entries.sort_by(|a, b| a.model_id.cmp(&b.model_id));
        let expected = vec![
            expected_entry(
                "claude-haiku-4-5",
                ProviderType::Anthropic,
                200_000,
                64_000,
                (true, false, true),
                None,
            ),
            expected_entry(
                "claude-sonnet-4-5",
                ProviderType::Anthropic,
                200_000,
                64_000,
                (true, true, true),
                Some((3.0, 15.0)),
            ),
            expected_entry(
                "gpt-4o",
                ProviderType::OpenAi,
                128_000,
                16_384,
                (true, false, true),
                None,
            ),
        ];
        assert_eq!(
            entries, expected,
            "未知 slug はスキップされ、部分価格のモデルは価格 None になる"
        );
    }

    // Given: HTTP 500 を返すモックサーバ
    // When: fetch を実行する
    // Then: ModelError::Fetch に変換される
    #[tokio::test(flavor = "multi_thread")]
    async fn models_dev_fetch_http_error_maps_to_model_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let fetcher = ReqwestModelsDevFetcher::new(server.uri());

        let err = fetcher.fetch().await.expect_err("HTTP 500 はエラーになる");

        assert!(
            matches!(err, ModelError::Fetch(_)),
            "Fetch へ変換される: {err:?}"
        );
    }

    // Given: 200 だが JSON でない本文を返すモックサーバ
    // When: fetch を実行する
    // Then: ModelError::Parse に変換される
    #[tokio::test(flavor = "multi_thread")]
    async fn models_dev_fetch_invalid_json_maps_to_parse_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_string("<not json>"))
            .mount(&server)
            .await;
        let fetcher = ReqwestModelsDevFetcher::new(server.uri());

        let err = fetcher
            .fetch()
            .await
            .expect_err("不正な JSON はエラーになる");

        assert!(
            matches!(err, ModelError::Parse(_)),
            "Parse へ変換される: {err:?}"
        );
    }
}
