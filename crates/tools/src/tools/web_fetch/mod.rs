//! web_fetch ツールの足場。

pub mod extract;
mod metadata;
mod truncate;

use std::sync::Arc;

use reqwest::header::CONTENT_LENGTH;
use scraper::Selector;
use serde::Deserialize;

use crate::error::ToolError;
use crate::network_guard::{GuardedResponse, NetworkGuard, NetworkGuardError};
use crate::result::ToolResult;
use crate::tool::{Permissions, Tool};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WebFetchArgs {
    url: String,
    selector: Option<String>,
    #[serde(default)]
    format: OutputFormat,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutputFormat {
    #[default]
    Text,
    Markdown,
    Html,
}

pub struct WebFetch {
    guard: Arc<NetworkGuard>,
}

impl WebFetch {
    pub fn new() -> Result<Self, NetworkGuardError> {
        Ok(Self {
            guard: Arc::new(NetworkGuard::new()?),
        })
    }

    pub fn with_guard(guard: Arc<NetworkGuard>) -> Self {
        Self { guard }
    }
}

#[async_trait::async_trait]
impl Tool for WebFetch {
    fn name(&self) -> &'static str {
        "web_fetch"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "url": { "type": "string" },
                "selector": { "type": "string" },
                "format": { "type": "string", "enum": ["text", "markdown", "html"], "default": "text" }
            },
            "required": ["url"],
            "additionalProperties": false
        })
    }

    fn permissions(&self) -> Permissions {
        Permissions::network()
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolResult, ToolError> {
        let args: WebFetchArgs =
            serde_json::from_value(args).map_err(|error| ToolError::InvalidArgs {
                detail: format!("web_fetch の引数を解析できませんでした: {error}"),
            })?;
        if let Some(selector) = args.selector.as_deref()
            && let Err(error) = Selector::parse(selector)
        {
            return Ok(ToolResult::error(format!("invalid selector: {error}")));
        }

        let response = match self.guard.get(&args.url).await {
            Ok(response) => response,
            Err(error) => {
                return Ok(ToolResult::error(error.to_string())
                    .with_detail(metadata::error_metadata(&args.url, &error)));
            }
        };
        Ok(process_response(&args, response))
    }
}

fn process_response(args: &WebFetchArgs, response: GuardedResponse) -> ToolResult {
    let html = String::from_utf8_lossy(&response.body);
    let (content, extraction_method) = match args.format {
        OutputFormat::Html => (html.into_owned(), "raw_html"),
        OutputFormat::Text | OutputFormat::Markdown => {
            let extracted = extract::ExtractionChain::standard(args.selector.as_deref()).run(
                &html,
                &args.url,
                args.format,
            );
            (extracted.content, extracted.method)
        }
    };
    let (content, truncation) = truncate::truncate_model_output(content);
    let content_length = response
        .headers
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    let metadata = metadata::WebFetchMetadata {
        url: args.url.clone(),
        final_url: response.final_url.to_string(),
        status_code: response.status.as_u16(),
        content_length,
        decompressed_bytes: response.body.len(),
        redirect_count: response.redirect_count,
        redirect_blocked: false,
        format: args.format,
        extraction_method: extraction_method.to_owned(),
        truncated: truncation.truncated,
        original_bytes: truncation.truncated.then_some(truncation.original_bytes),
        truncation_hint: truncation.truncated.then_some(metadata::TRUNCATION_HINT),
    };
    ToolResult::success(content).with_detail(metadata.to_detail())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_is_web_fetch() {
        let guard = NetworkGuard::new().expect("guard");
        assert_eq!(WebFetch::with_guard(Arc::new(guard)).name(), "web_fetch");
    }

    #[test]
    fn schema_lists_url_selector_format_only() {
        let guard = NetworkGuard::new().expect("guard");
        assert_eq!(
            WebFetch::with_guard(Arc::new(guard)).schema(),
            serde_json::json!({
                "type": "object",
                "properties": {
                    "url": { "type": "string" },
                    "selector": { "type": "string" },
                    "format": { "type": "string", "enum": ["text", "markdown", "html"], "default": "text" }
                },
                "required": ["url"],
                "additionalProperties": false
            })
        );
    }

    #[test]
    fn args_reject_unknown_fields() {
        assert!(
            serde_json::from_value::<WebFetchArgs>(
                serde_json::json!({"url": "https://x", "extra": true})
            )
            .is_err()
        );
    }

    #[test]
    fn format_defaults_to_text() {
        assert_eq!(
            serde_json::from_value::<WebFetchArgs>(serde_json::json!({"url": "https://x"}))
                .expect("args")
                .format,
            OutputFormat::Text
        );
        assert_eq!(
            serde_json::from_value::<WebFetchArgs>(
                serde_json::json!({"url": "https://x", "format": "markdown"})
            )
            .expect("args")
            .format,
            OutputFormat::Markdown
        );
        assert_eq!(
            serde_json::from_value::<WebFetchArgs>(
                serde_json::json!({"url": "https://x", "format": "html"})
            )
            .expect("args")
            .format,
            OutputFormat::Html
        );
    }

    #[test]
    fn permissions_declare_network() {
        let guard = NetworkGuard::new().expect("guard");
        assert_eq!(
            WebFetch::with_guard(Arc::new(guard)).permissions(),
            Permissions::network()
        );
    }
}
