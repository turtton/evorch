use reqwest::header::HeaderMap;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::time::Duration;

/// Endpoint configuration. The label is an operator identifier, never a URL or credential.
pub struct McpClientConfig {
    pub server_label: String,
    pub endpoint: String,
    pub extra_headers: HeaderMap,
    pub timeout: Duration,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpClientInfo {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct McpToolDefinition {
    pub name: String,
    pub description: Option<String>,
    pub input_schema: Map<String, Value>,
}

/// Supported MCP content blocks; unknown variants fail closed during deserialization.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum McpToolContent {
    Text {
        text: String,
    },
    Image {
        data: String,
        #[serde(rename = "mimeType")]
        mime_type: String,
    },
    Audio {
        data: String,
        #[serde(rename = "mimeType")]
        mime_type: String,
    },
    Resource {
        resource: Map<String, Value>,
    },
    ResourceLink {
        uri: String,
        name: String,
    },
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpToolResult {
    pub content: Vec<McpToolContent>,
    #[serde(default)]
    pub is_error: bool,
    pub structured_content: Option<Map<String, Value>>,
}

impl McpToolResult {
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(|item| match item {
                McpToolContent::Text { text } => Some(text.as_str()),
                McpToolContent::Image { .. }
                | McpToolContent::Audio { .. }
                | McpToolContent::Resource { .. }
                | McpToolContent::ResourceLink { .. } => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct InitializeResult {
    pub protocol_version: String,
    pub capabilities: Map<String, Value>,
    pub server_info: McpClientInfo,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ToolsPage {
    pub tools: Vec<McpToolDefinition>,
    pub next_cursor: Option<String>,
}
