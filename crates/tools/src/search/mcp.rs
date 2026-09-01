//! keyless MCP JSON-RPC 通信の transport。
//!
//! design lock（OpenCode V2 同型 single-shot）: 1 call につき 1 回の HTTP POST
//! `tools/call` のみで、initialize handshake・`Mcp-Session-Id`・`tools/list` は行わない。

use std::sync::Arc;

use async_trait::async_trait;
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderMap, HeaderValue};
use serde_json::Value;

use super::envelope::parse_envelope;
use super::error::SearchError;
use crate::network_guard::{NetworkGuard, NetworkGuardError};

/// 単発 design lock における JSON-RPC request id。
pub const REQUEST_ID: i64 = 1;

/// MCP tools/call の成功結果。
#[derive(Debug, Clone)]
pub struct McpToolSuccess {
    /// provider が返した検索結果の text。
    pub text: String,
    /// content item に付与されていた provider 固有の `_meta`（存在する場合）。
    pub usage: Option<Value>,
}

/// MCP endpoint への単発 tools/call を担う transport 境界。
#[async_trait]
pub trait McpTransport: Send + Sync {
    /// 単発の tools/call を送信し、成功結果を返す。
    ///
    /// # Errors
    /// 通信・HTTP status・envelope 解析・provider 拒否のいずれかに失敗した場合、
    /// 対応する [`SearchError`] を返す。
    async fn call_tool(
        &self,
        tool_name: &str,
        arguments: Value,
    ) -> Result<McpToolSuccess, SearchError>;
}

/// [`NetworkGuard`] 経由で MCP endpoint に単発 tools/call を送る transport。
pub struct NetworkGuardMcpTransport {
    guard: Arc<NetworkGuard>,
    endpoint: String,
    extra_headers: HeaderMap,
}

impl NetworkGuardMcpTransport {
    /// guard・endpoint・provider 固有の追加 header から transport を構築する。
    pub fn new(
        guard: Arc<NetworkGuard>,
        endpoint: impl Into<String>,
        extra_headers: HeaderMap,
    ) -> Self {
        Self {
            guard,
            endpoint: endpoint.into(),
            extra_headers,
        }
    }
}

#[async_trait]
impl McpTransport for NetworkGuardMcpTransport {
    async fn call_tool(
        &self,
        tool_name: &str,
        arguments: Value,
    ) -> Result<McpToolSuccess, SearchError> {
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": REQUEST_ID,
            "method": "tools/call",
            "params": {
                "name": tool_name,
                "arguments": arguments,
            },
        });
        let mut headers = self.extra_headers.clone();
        headers.insert(
            ACCEPT,
            HeaderValue::from_static("application/json, text/event-stream"),
        );
        let response = self
            .guard
            .post_json(&self.endpoint, headers, &body)
            .await
            .map_err(map_guard_error)?;
        if !response.status.is_success() {
            return Err(SearchError::HttpStatus(response.status.as_u16()));
        }
        let content_type = response
            .headers
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        parse_envelope(REQUEST_ID, content_type.as_deref(), &response.body)
    }
}

/// NetworkGuard の失敗を SearchError へ写像する。
///
/// reqwest の timeout だけを fallback 対象の [`SearchError::Timeout`] にし、
/// それ以外は fail-closed で [`SearchError::Transport`] にする。
fn map_guard_error(error: NetworkGuardError) -> SearchError {
    if let NetworkGuardError::Http(inner) = &error
        && inner.is_timeout()
    {
        return SearchError::Timeout;
    }
    SearchError::Transport(error.to_string())
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};

    use super::*;

    // Given: BlockedIp の NetworkGuardError / When: SearchError へ写像する / Then: fail-closed の Transport になる
    #[test]
    fn maps_guard_error_to_transport() {
        let error = NetworkGuardError::BlockedIp {
            addr: IpAddr::V4(Ipv4Addr::new(169, 254, 169, 254)),
        };

        let mapped = map_guard_error(error);

        assert!(matches!(mapped, SearchError::Transport(_)));
    }
}
