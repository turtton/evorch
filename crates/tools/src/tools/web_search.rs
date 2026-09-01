//! web_search ツールの実装。
//!
//! Exa keyless endpoint を primary、Tavily keyless endpoint を 1 回限りの
//! fallback とする keyless 検索ツールである。API key による keyed transport は
//! 将来拡張 (interview Q2) であり、本実装は環境変数の存在確認のみを行う。
//! キーが環境に存在しても現行 transport はそれを使用しないため、資格情報の
//! 状態は "key_present_unused" として報告する。メタデータに API key の値が
//! 含まれることはなく、credential_status はリテラル値のみである。

use std::sync::Arc;
use std::time::Instant;

use serde::Deserialize;

use crate::error::ToolError;
use crate::network_guard::{NetworkGuard, NetworkGuardError};
use crate::result::ToolResult;
use crate::search::{
    ExaKeylessProvider, SearchError, SearchOptions, SearchProvider, SearchResults,
    TavilyKeylessProvider,
};
use crate::tool::{Permissions, Tool};

/// 環境に API key が存在しないことを表す credential_status。
const CREDENTIAL_STATUS_KEYLESS: &str = "keyless";
/// API key が存在するが現行 transport では未使用であることを表す credential_status。
const CREDENTIAL_STATUS_KEY_PRESENT_UNUSED: &str = "key_present_unused";
/// credential_status の判定対象となる環境変数名。
const CREDENTIAL_ENV_KEYS: [&str; 2] = ["EXA_API_KEY", "TAVILY_API_KEY"];

/// web_search の引数（schema 契約の mirror）。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WebSearchArgs {
    query: String,
    max_results: Option<u32>,
}

/// ToolExecutor の detail として報告する検索メタデータ。
#[derive(Debug, serde::Serialize)]
struct WebSearchMetadata {
    provider: String,
    request_id: Option<String>,
    latency_ms: u64,
    result_count: usize,
    used_fallback: bool,
    fallback_attempts: u32,
    credential_status: &'static str,
    usage: Option<serde_json::Value>,
}

/// 検索フローの結果。
enum Flight {
    /// primary が成功した。
    Primary(SearchResults),
    /// fallback が 1 回試行されて成功した。
    Fallback(SearchResults),
    /// primary が fallback 非対象の error で失敗した（fallback は試行しない）。
    PrimaryOnly(SearchError),
    /// primary の fallback 対象 error に対し、fallback も失敗した。
    Both {
        primary: SearchError,
        fallback: SearchError,
    },
}

/// credential_status 判定に使う環境変数 lookup。
type EnvLookup = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// Exa primary・Tavily fallback の keyless web 検索ツール。
///
/// fallback は fallback 対象の error（429・5xx・timeout、
/// [`SearchError::is_fallback_trigger`]）に対してのみ 1 回試行され、fallback の
/// 結果が再び fallback 対象でも連鎖しない。
pub struct WebSearch {
    primary: Arc<dyn SearchProvider>,
    fallback: Arc<dyn SearchProvider>,
    env_lookup: EnvLookup,
}

impl WebSearch {
    /// production 用の既定構成（Exa keyless primary・Tavily keyless fallback）で
    /// 構築する。
    ///
    /// 1 つの [`NetworkGuard`] を両 provider の guarded transport で共有し、
    /// credential 判定には `std::env::var` の実参照を使う。
    ///
    /// # Errors
    /// [`NetworkGuard::new`] が DNS resolver を初期化できなかった場合、
    /// [`NetworkGuardError`] を返す。
    pub fn keyless_default() -> Result<Self, NetworkGuardError> {
        let guard = Arc::new(NetworkGuard::new()?);
        let primary = Arc::new(ExaKeylessProvider::with_guard(Arc::clone(&guard)));
        let fallback = Arc::new(TavilyKeylessProvider::with_guard(guard));
        Ok(Self::for_providers(primary, fallback))
    }

    /// provider を注入して構築する（credential 判定は実環境変数を参照する）。
    pub fn for_providers(
        primary: Arc<dyn SearchProvider>,
        fallback: Arc<dyn SearchProvider>,
    ) -> Self {
        Self::for_providers_with_env_lookup(
            primary,
            fallback,
            Arc::new(|key: &str| std::env::var(key).ok()),
        )
    }

    /// 既定配線の診断・検証用に、primary と fallback の provider 識別名を返す。
    ///
    /// 戻り値は metadata の `provider` field と同一の情報源であり、production
    /// 既定構成 (`keyless_default`) では `("exa", "tavily")` となる。
    pub fn provider_names(&self) -> (&str, &str) {
        (self.primary.name(), self.fallback.name())
    }

    /// provider と credential 判定用の環境変数 lookup をすべて注入して構築する。
    pub fn for_providers_with_env_lookup(
        primary: Arc<dyn SearchProvider>,
        fallback: Arc<dyn SearchProvider>,
        env_lookup: EnvLookup,
    ) -> Self {
        Self {
            primary,
            fallback,
            env_lookup,
        }
    }

    /// 環境変数の存在から credential_status を判定する。
    ///
    /// keyed transport は将来拡張のため、キーが存在しても現行経路では未使用である。
    /// 空文字列のキーは存在しないものと扱う。
    fn credential_status(&self) -> &'static str {
        let has_key = CREDENTIAL_ENV_KEYS
            .iter()
            .any(|key| (self.env_lookup)(key).is_some_and(|value| !value.is_empty()));
        if has_key {
            CREDENTIAL_STATUS_KEY_PRESENT_UNUSED
        } else {
            CREDENTIAL_STATUS_KEYLESS
        }
    }
}

#[async_trait::async_trait]
impl Tool for WebSearch {
    fn name(&self) -> &'static str {
        "web_search"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "minLength": 1 },
                "max_results": { "type": "integer", "minimum": 1, "maximum": 10 }
            },
            "required": ["query"],
            "additionalProperties": false
        })
    }

    fn permissions(&self) -> Permissions {
        Permissions::network()
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolResult, ToolError> {
        // Executor 経由では schema 検証済み。直接呼び出しの防御として serde で
        // 境界パースし、不適合は InvalidArgs にする。
        let args: WebSearchArgs =
            serde_json::from_value(args).map_err(|error| ToolError::InvalidArgs {
                detail: format!("web_search の引数を解析できませんでした: {error}"),
            })?;
        let started = Instant::now();
        let options = SearchOptions {
            max_results: args.max_results,
        };

        let flight = match self.primary.search(&args.query, &options).await {
            Ok(results) => Flight::Primary(results),
            Err(error) if error.is_fallback_trigger() => {
                // fallback は 1 回だけ試行し、その結果が再び fallback 対象でも連鎖させない。
                match self.fallback.search(&args.query, &options).await {
                    Ok(results) => Flight::Fallback(results),
                    Err(fallback_error) => Flight::Both {
                        primary: error,
                        fallback: fallback_error,
                    },
                }
            }
            Err(error) => Flight::PrimaryOnly(error),
        };
        let latency_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let credential_status = self.credential_status();

        let metadata = match &flight {
            Flight::Primary(results) => WebSearchMetadata {
                provider: self.primary.name().to_owned(),
                request_id: results.request_id.clone(),
                latency_ms,
                result_count: results.result_count,
                used_fallback: false,
                fallback_attempts: 0,
                credential_status,
                usage: results.usage.clone(),
            },
            Flight::Fallback(results) => WebSearchMetadata {
                provider: self.fallback.name().to_owned(),
                request_id: results.request_id.clone(),
                latency_ms,
                result_count: results.result_count,
                used_fallback: true,
                fallback_attempts: 1,
                credential_status,
                usage: results.usage.clone(),
            },
            Flight::PrimaryOnly(_) => WebSearchMetadata {
                provider: self.primary.name().to_owned(),
                request_id: None,
                latency_ms,
                result_count: 0,
                used_fallback: false,
                fallback_attempts: 0,
                credential_status,
                usage: None,
            },
            Flight::Both { .. } => WebSearchMetadata {
                provider: self.fallback.name().to_owned(),
                request_id: None,
                latency_ms,
                result_count: 0,
                used_fallback: true,
                fallback_attempts: 1,
                credential_status,
                usage: None,
            },
        };
        let detail = metadata_detail(metadata);

        match flight {
            Flight::Primary(results) | Flight::Fallback(results) => {
                Ok(ToolResult::success(results.content).with_detail(detail))
            }
            Flight::PrimaryOnly(error) => {
                let content = format!(
                    "web_search が失敗しました ({}): {error}",
                    self.primary.name()
                );
                Ok(ToolResult::error(content).with_detail(detail))
            }
            Flight::Both { primary, fallback } => {
                let content = format!(
                    "web_search が失敗しました: primary({}): {primary}; fallback({}): {fallback}",
                    self.primary.name(),
                    self.fallback.name()
                );
                Ok(ToolResult::error(content).with_detail(detail))
            }
        }
    }
}

/// metadata を detail 用の JSON 値へ変換する。
// SAFE-EXPECT: WebSearchMetadata は文字列・数値・null のみで構成され serde_json::to_value は失敗しない。
fn metadata_detail(metadata: WebSearchMetadata) -> serde_json::Value {
    serde_json::to_value(metadata).expect("WebSearchMetadata の serialize は失敗しない")
}
