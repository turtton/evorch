//! Shared external MCP transport boundary. exchange and cancellation use &self so
//! a supervisor/side channel can cancel while a call is awaiting its response.
use crate::mcp::{
    McpError, McpErrorKind,
    types::{
        ApprovalPermit, MAX_FRAME_BYTES, MAX_FRAMES_PER_EXCHANGE, MAX_REQUEST_BYTES,
        MAX_RESPONSE_HEADER_BYTES, ResolvedMcpServer,
    },
};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{fmt, path::Path, sync::Arc};
use tokio::sync::watch;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProtocolEra {
    Modern,
    Legacy,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProtocolVersion {
    #[serde(rename = "2026-07-28")]
    V2026_07_28,
    #[serde(rename = "2025-11-25")]
    V2025_11_25,
    #[serde(rename = "2025-06-18")]
    V2025_06_18,
    #[serde(rename = "2025-03-26")]
    V2025_03_26,
    /// Only stdio; the old HTTP+SSE transport is not implemented.
    #[serde(rename = "2024-11-05")]
    V2024_11_05,
}
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RpcId {
    Number(i64),
    String(String),
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RpcRequest {
    pub jsonrpc: String,
    pub id: RpcId,
    pub method: String,
    pub params: Value,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RpcNotification {
    pub jsonrpc: String,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}
/// Raw structured errors remain internal; never retain in McpError/status/events.
#[derive(Clone, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RpcFrame {
    Result {
        jsonrpc: String,
        id: RpcId,
        result: Value,
    },
    Error {
        jsonrpc: String,
        id: Option<RpcId>,
        error: RpcError,
    },
    Request(RpcRequest),
    Notification(RpcNotification),
}
macro_rules! redacted_debug {
    ($($ty:ty),+ $(,)?) => { $(impl fmt::Debug for $ty {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str(concat!(stringify!($ty), "(<redacted>)")) }
    })+ };
}
redacted_debug!(RpcRequest, RpcNotification, RpcError, RpcFrame);
/// Only headers necessary for protocol state. Not Serialize/Debug (session is secret).
#[derive(Default)]
pub struct McpResponseHeaders {
    pub protocol_version: Option<String>,
    pub session_id: Option<sandbox::Secret>,
    pub content_type: Option<String>,
}
/// Bounded completion envelope keeps HTTP 400 + modern RPC error together.
/// For SSE return at the matching final response, not at transport EOF.
pub struct McpResponseEnvelope {
    pub status: Option<u16>,
    pub headers: McpResponseHeaders,
    frames: Vec<RpcFrame>,
    pub received_bytes: usize,
}
impl McpResponseEnvelope {
    pub fn new(
        status: Option<u16>,
        headers: McpResponseHeaders,
        frames: Vec<RpcFrame>,
        received_bytes: usize,
    ) -> Result<Self, McpErrorKind> {
        let header_bytes = headers.protocol_version.as_ref().map_or(0, String::len)
            + headers
                .session_id
                .as_ref()
                .map_or(0, |value| value.expose().len())
            + headers.content_type.as_ref().map_or(0, String::len);
        if header_bytes > MAX_RESPONSE_HEADER_BYTES || frames.len() > MAX_FRAMES_PER_EXCHANGE {
            return Err(McpErrorKind::LimitExceeded);
        }
        if received_bytes > MAX_REQUEST_BYTES {
            return Err(McpErrorKind::LimitExceeded);
        }
        let mut total = 0usize;
        for frame in &frames {
            let size = serde_json::to_vec(frame)
                .map_err(|_| McpErrorKind::Protocol)?
                .len();
            if size > MAX_FRAME_BYTES {
                return Err(McpErrorKind::LimitExceeded);
            }
            total = total.checked_add(size).ok_or(McpErrorKind::LimitExceeded)?;
            if total > MAX_REQUEST_BYTES {
                return Err(McpErrorKind::LimitExceeded);
            }
        }
        Ok(Self {
            status,
            headers,
            frames,
            received_bytes,
        })
    }
    pub fn frames(&self) -> &[RpcFrame] {
        &self.frames
    }
    pub fn into_frames(self) -> Vec<RpcFrame> {
        self.frames
    }
}
redacted_debug!(McpResponseHeaders, McpResponseEnvelope);
/// Sticky cancellation. watch avoids lost notifications even before first await.
#[derive(Debug, Clone)]
pub struct CancelContext {
    signal: watch::Sender<bool>,
}
impl Default for CancelContext {
    fn default() -> Self {
        Self {
            signal: watch::channel(false).0,
        }
    }
}
impl CancelContext {
    pub fn cancel(&self) {
        self.signal.send_replace(true);
    }
    pub fn is_cancelled(&self) -> bool {
        *self.signal.borrow()
    }
    pub async fn cancelled(&self) {
        let mut rx = self.signal.subscribe();
        let _ = rx.wait_for(|value| *value).await;
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransportKind {
    Stdio,
    Http,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestPhase {
    Initialize,
    Request,
}
#[derive(Debug, Clone)]
pub struct CancelRequest {
    pub id: RpcId,
    pub era: ProtocolEra,
    pub transport: TransportKind,
    pub phase: RequestPhase,
    pub issued: bool,
    pub in_flight: bool,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CancellationAction {
    None,
    Close,
    NotifyAndFlushThenClose,
    NotifyBestEffort,
}
pub fn cancellation_action(context: &CancelRequest) -> CancellationAction {
    if !context.issued || !context.in_flight {
        return CancellationAction::None;
    }
    if context.era == ProtocolEra::Legacy && context.phase == RequestPhase::Initialize {
        return CancellationAction::Close;
    }
    match (context.era, context.transport) {
        (_, TransportKind::Stdio) => CancellationAction::NotifyAndFlushThenClose,
        (ProtocolEra::Modern, TransportKind::Http) => CancellationAction::Close,
        (ProtocolEra::Legacy, TransportKind::Http) => CancellationAction::NotifyBestEffort,
    }
}
#[derive(Debug, Clone, Copy)]
pub enum ShutdownReason {
    Completed,
    Cancelled,
    ApprovalRevoked,
    InitializationFailed,
    OwnerDropped,
}
#[async_trait]
pub trait McpTransport: Send + Sync {
    async fn exchange(
        &self,
        request: RpcRequest,
        cancel: CancelContext,
    ) -> Result<McpResponseEnvelope, McpError>;
    /// Writer/supervisor side channel, never the in-flight call mutex.
    async fn notify(&self, notification: RpcNotification) -> Result<(), McpError>;
    /// Legacy only: reject unsupported server requests with a protocol error.
    /// Modern transports must refuse outgoing JSON-RPC responses.
    async fn respond(&self, response: RpcFrame) -> Result<(), McpError>;
    async fn cancel(&self, request: CancelRequest) -> Result<(), McpError>;
    /// Close stdin, grace/TERM/grace/KILL/reap (stdio), close request streams (HTTP).
    /// Cleanup continues after broken pipes; Drop must schedule a cleanup fallback.
    async fn shutdown(&self, reason: ShutdownReason) -> Result<(), McpError>;
}
/// Permit lifetime extends through reap, including GUI tests and restored sessions.
pub trait McpProcessPermit: Send + Sync {}
pub trait McpProcessLimiter: Send + Sync {
    fn try_acquire(&self, canonical_root: &Path)
    -> Result<Box<dyn McpProcessPermit>, McpErrorKind>;
}
#[async_trait]
pub trait McpTransportFactory: Send + Sync {
    /// Must validate permit scope/fingerprint/revocation before ANY spawn/DNS/connect.
    /// For stdio acquire the shared limiter before spawn; release only after reap.
    async fn connect(
        &self,
        server: ResolvedMcpServer,
        permit: ApprovalPermit,
        limiter: Arc<dyn McpProcessLimiter>,
        cancel: CancelContext,
    ) -> Result<Arc<dyn McpTransport>, McpError>;
}
