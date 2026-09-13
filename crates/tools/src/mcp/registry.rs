use super::{
    McpClient, McpClientConfig, McpClientInfo, McpError, McpErrorKind, McpTool, McpToolDefinition,
    McpToolResult,
};
use crate::NetworkGuard;
use std::sync::Arc;
use tokio::sync::{Mutex, OnceCell};

struct Session {
    client: Mutex<McpClient>,
    definitions: Vec<McpToolDefinition>,
}

/// Constructor-only registration for an operator-approved endpoint and headers.
/// Construction is inert. `discover` performs I/O and requires startup approval;
/// `tool` uses a previously approved definition and defers ALL I/O until execution.
pub struct McpToolRegistry {
    guard: Arc<NetworkGuard>,
    config: McpClientConfig,
    info: McpClientInfo,
    session: OnceCell<Session>,
}

impl McpToolRegistry {
    pub const fn new(
        guard: Arc<NetworkGuard>,
        config: McpClientConfig,
        info: McpClientInfo,
    ) -> Self {
        Self {
            guard,
            config,
            info,
            session: OnceCell::const_new(),
        }
    }

    /// Discover only after the caller approves startup communication.
    pub async fn discover(self: &Arc<Self>) -> Result<Vec<McpTool>, McpError> {
        Ok(self
            .session()
            .await?
            .definitions
            .iter()
            .cloned()
            .map(|definition| self.tool(definition))
            .collect())
    }

    /// Register cached metadata without connecting; live metadata must match before call.
    pub fn tool(self: &Arc<Self>, definition: McpToolDefinition) -> McpTool {
        McpTool::new(definition, Arc::clone(self))
    }

    async fn session(&self) -> Result<&Session, McpError> {
        self.session
            .get_or_try_init(|| async {
                let config = McpClientConfig {
                    server_label: self.config.server_label.clone(),
                    endpoint: self.config.endpoint.clone(),
                    extra_headers: self.config.extra_headers.clone(),
                    timeout: self.config.timeout,
                };
                let mut client =
                    McpClient::connect(Arc::clone(&self.guard), config, self.info.clone()).await?;
                let definitions = client.list_tools().await?;
                Ok(Session {
                    client: Mutex::new(client),
                    definitions,
                })
            })
            .await
    }

    pub(super) async fn call(
        &self,
        definition: &McpToolDefinition,
        args: serde_json::Value,
    ) -> Result<(McpToolResult, serde_json::Value), McpError> {
        let session = self.session().await?;
        if !session.definitions.iter().any(|live| {
            live.name == definition.name && live.input_schema == definition.input_schema
        }) {
            return Err(McpError {
                server: self.config.server_label.clone(),
                method: "tools/list",
                request_id: None,
                kind: McpErrorKind::Configuration,
            });
        }
        let mut client = session.client.lock().await;
        let result = client.call_tool(&definition.name, args).await?;
        Ok((result, client.call_success_detail()))
    }
}
