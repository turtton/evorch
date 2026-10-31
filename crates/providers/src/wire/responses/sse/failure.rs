//! stream 内の失敗イベント (`response.failed` とトップレベル `error`) の解釈と分類。
//!
//! 受け入れる形は dialect に依らず共通です。`error` イベントはトップレベルの
//! `code`/`message` と入れ子の `error.{code,message,headers}` の両方、`response.failed` は
//! `response.error.{code,message,headers}` を読みます。分類だけが dialect で異なります。
//!
//! evorch は失敗イベントで即終了します。Codex CLI はトップレベル `error` の一部の code
//! (`flex_unavailable`, `model_not_found`) だけを終端とし、ほかは stream を続けますが、
//! evorch は全 code で終端します。

use std::time::Duration;

use serde_json::Value;

use crate::ResponsesDialect;
use crate::error::ProviderError;

/// 失敗イベントから取り出した、分類に必要な情報。
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Failure {
    code: Option<String>,
    message: Option<String>,
    retry_after: Option<Duration>,
}

impl Failure {
    /// `response.failed` の `response.error` から読みます。
    pub(super) fn from_failed_event(event: &Value) -> Self {
        Self::from_error_object(event.get("response").and_then(|r| r.get("error")))
    }

    /// トップレベル `error` イベントから読みます。入れ子の `error` が優先で、
    /// 欠けた `code`/`message` はトップレベルの値で補います。トップレベルの `headers` は
    /// 別用途の metadata なので `retry-after` として読みません。
    pub(super) fn from_error_event(event: &Value) -> Self {
        let nested = Self::from_error_object(event.get("error"));
        let top = Self::from_error_object(Some(event));
        Self {
            code: nested.code.or(top.code),
            message: nested.message.or(top.message),
            retry_after: nested.retry_after,
        }
    }

    fn from_error_object(error: Option<&Value>) -> Self {
        let text = |key: &str| error?.get(key).and_then(Value::as_str).map(str::to_owned);
        Self {
            code: text("code"),
            // `"error": "message"` のように文字列だけを返す実装も許容する。
            message: text("message").or_else(|| error?.as_str().map(str::to_owned)),
            retry_after: error
                .and_then(|error| error.get("headers"))
                .and_then(crate::retry_after::from_json_headers),
        }
    }

    /// 失敗を HTTP 応答と同じ再試行・routing 分類へ正規化します。
    ///
    /// SSE は HTTP 200 で始まるため、status の代わりに code で障害を見分けます。
    /// 合成した status でも本文の先頭は `code: message` とし、`server_is_overloaded` の
    /// 再試行判定や structured output 非対応判定が code を参照できるようにします。
    pub(super) fn into_error(self, dialect: ResponsesDialect) -> ProviderError {
        let body = match (&self.code, &self.message) {
            (Some(code), Some(message)) => format!("{code}: {message}"),
            (Some(code), None) => code.clone(),
            (None, Some(message)) => message.clone(),
            (None, None) => "stream failure event received".to_owned(),
        };
        let http = |status| ProviderError::Http {
            status,
            body: body.clone(),
        };
        match (dialect, self.code.as_deref()) {
            (_, Some("server_error")) => http(500),
            (ResponsesDialect::OpenAi, Some("rate_limit_exceeded")) => {
                ProviderError::RateLimited { retry_after: None }
            }
            (ResponsesDialect::OpenAi, _) => http(400),
            (ResponsesDialect::Codex, Some("rate_limit_exceeded" | "slow_down")) => {
                ProviderError::RateLimited {
                    retry_after: self.retry_after.or_else(|| {
                        self.message
                            .as_deref()
                            .and_then(crate::retry_after::from_message)
                    }),
                }
            }
            (ResponsesDialect::Codex, Some("policy_capacity_unavailable")) => http(503),
            (
                ResponsesDialect::Codex,
                Some(
                    "insufficient_quota"
                    | "credit_balance_exhausted"
                    | "organization_spend_limit_exceeded"
                    | "project_spend_limit_exceeded"
                    | "usage_not_included"
                    | "usage_limit_reached",
                ),
            ) => http(402),
            (ResponsesDialect::Codex, Some("model_not_found")) => http(404),
            // server_is_overloaded は retry.rs の特例 (HTTP 400 + code) に任せ、
            // context_length_exceeded・各種 policy・invalid_prompt・未知の code も
            // 再試行しない 400 とする。
            (ResponsesDialect::Codex, _) => http(400),
        }
    }
}
