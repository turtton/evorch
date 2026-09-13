use super::types::{InitializeResult, ToolsPage};
use super::wire::{accept_headers, request, response_frames};
use super::{
    McpClientConfig, McpClientInfo, McpError, McpErrorKind, McpToolDefinition, McpToolResult,
};
use crate::{GuardedResponse, NetworkGuard, NetworkGuardError};
use reqwest::header::{CONTENT_TYPE, HeaderValue};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::{collections::HashSet, sync::Arc};

const PROTOCOL_VERSION: &str = "2025-03-26";

/// A successfully initialized session. Calls are serialized with unique session-local ids.
pub struct McpClient {
    guard: Arc<NetworkGuard>,
    config: McpClientConfig,
    next_id: i64,
}

impl McpClient {
    /// Completes initialization before making the client available to callers.
    /// Cancellation or failure drops the incomplete client; no automatic retry occurs.
    pub async fn connect(
        guard: Arc<NetworkGuard>,
        config: McpClientConfig,
        info: McpClientInfo,
    ) -> Result<Self, McpError> {
        if config.server_label.is_empty()
            || config.server_label.len() > 64
            || !config
                .server_label
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"_- .".contains(&byte))
            || config.timeout.is_zero()
        {
            return Err(McpError {
                server: "invalid-label".into(),
                method: "initialize",
                request_id: None,
                kind: McpErrorKind::Configuration,
            });
        }
        let mut client = Self {
            guard,
            config,
            next_id: 1,
        };
        client.config.extra_headers.remove("mcp-session-id");
        client.config.extra_headers.remove("mcp-protocol-version");
        let id = client.allocate_id("initialize")?;
        let body = request(
            id,
            "initialize",
            json!({"protocolVersion":PROTOCOL_VERSION,"capabilities":{},"clientInfo":info}),
        );
        let response = client.post("initialize", &body).await?;
        let result: InitializeResult = client.decode("initialize", id, &response)?;
        if result.protocol_version != PROTOCOL_VERSION
            || result.server_info.name.is_empty()
            || !result.capabilities.contains_key("tools")
        {
            return Err(client.error("initialize", Some(id), McpErrorKind::Protocol));
        }
        if let Some(session) = response.headers.get("mcp-session-id") {
            if !session
                .as_bytes()
                .iter()
                .all(|byte| (0x21..=0x7e).contains(byte))
                || session.is_empty()
            {
                return Err(client.error("initialize", Some(id), McpErrorKind::Protocol));
            }
            client
                .config
                .extra_headers
                .insert("mcp-session-id", session.clone());
        }
        client.config.extra_headers.insert(
            "mcp-protocol-version",
            HeaderValue::from_static(PROTOCOL_VERSION),
        );
        client
            .post(
                "notifications/initialized",
                &json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
            )
            .await?;
        Ok(client)
    }

    pub async fn list_tools(&mut self) -> Result<Vec<McpToolDefinition>, McpError> {
        let mut tools = Vec::new();
        let mut cursors = HashSet::new();
        let mut params = json!({});
        loop {
            let page: ToolsPage = self.rpc("tools/list", params).await?;
            tools.extend(page.tools);
            match page.next_cursor {
                None => return Ok(tools),
                Some(cursor) => {
                    if !cursors.insert(cursor.clone()) || cursors.len() >= 100 {
                        return Err(self.error(
                            "tools/list",
                            Some(self.next_id - 1),
                            McpErrorKind::Protocol,
                        ));
                    }
                    params = json!({"cursor":cursor});
                }
            }
        }
    }

    pub async fn call_tool(
        &mut self,
        name: &str,
        arguments: Value,
    ) -> Result<McpToolResult, McpError> {
        if !arguments.is_object() {
            return Err(self.error("tools/call", None, McpErrorKind::Configuration));
        }
        let result: McpToolResult = self
            .rpc("tools/call", json!({"name":name,"arguments":arguments}))
            .await?;
        if result.is_error {
            return Err(self.error(
                "tools/call",
                Some(self.next_id - 1),
                McpErrorKind::ToolRejected,
            ));
        }
        Ok(result)
    }

    fn allocate_id(&mut self, method: &'static str) -> Result<i64, McpError> {
        let id = self.next_id;
        self.next_id = id
            .checked_add(1)
            .ok_or_else(|| self.error(method, None, McpErrorKind::Protocol))?;
        Ok(id)
    }

    async fn rpc<T: DeserializeOwned>(
        &mut self,
        method: &'static str,
        params: Value,
    ) -> Result<T, McpError> {
        let id = self.allocate_id(method)?;
        let response = self.post(method, &request(id, method, params)).await?;
        self.decode(method, id, &response)
    }

    async fn post(&self, method: &'static str, body: &Value) -> Result<GuardedResponse, McpError> {
        let id = body.get("id").and_then(Value::as_i64);
        let headers = accept_headers(self.config.extra_headers.clone());
        let response = tokio::time::timeout(
            self.config.timeout,
            self.guard.post_json(&self.config.endpoint, headers, body),
        )
        .await
        .map_err(|_| self.error(method, id, McpErrorKind::Timeout))?
        .map_err(|error| {
            let kind = match error {
                NetworkGuardError::Http(inner) if inner.is_timeout() => McpErrorKind::Timeout,
                _ => McpErrorKind::Transport,
            };
            self.error(method, id, kind)
        })?;
        if !response.status.is_success() {
            return Err(self.error(
                method,
                id,
                McpErrorKind::HttpStatus(response.status.as_u16()),
            ));
        }
        Ok(response)
    }

    fn decode<T: DeserializeOwned>(
        &self,
        method: &'static str,
        id: i64,
        response: &GuardedResponse,
    ) -> Result<T, McpError> {
        let protocol_error = || self.error(method, Some(id), McpErrorKind::Protocol);
        let content_type = response
            .headers
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok());
        let frames = response_frames(content_type, &response.body).map_err(|_| protocol_error())?;
        let mut matching = frames
            .into_iter()
            .filter(|frame| frame.get("id").and_then(Value::as_i64) == Some(id));
        let frame = matching.next().ok_or_else(protocol_error)?;
        if matching.next().is_some() || frame.get("jsonrpc").and_then(Value::as_str) != Some("2.0")
        {
            return Err(protocol_error());
        }
        match (frame.get("result"), frame.get("error")) {
            (Some(result), None) => {
                serde_json::from_value(result.clone()).map_err(|_| protocol_error())
            }
            (None, Some(error))
                if error.get("code").and_then(Value::as_i64).is_some()
                    && error.get("message").and_then(Value::as_str).is_some() =>
            {
                Err(self.error(method, Some(id), McpErrorKind::ServerRejected))
            }
            _ => Err(protocol_error()),
        }
    }

    fn error(&self, method: &'static str, request_id: Option<i64>, kind: McpErrorKind) -> McpError {
        McpError {
            server: self.config.server_label.clone(),
            method,
            request_id,
            kind,
        }
    }
}
