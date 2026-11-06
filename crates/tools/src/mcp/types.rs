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

// ---- External project MCP contracts (Worker E owns shared types) ----
use config::mcp::{SourceRevision, ValueSource};
use sandbox::{CredentialStore, Secret};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, fmt, net::IpAddr, path::PathBuf, sync::Arc};

pub const MAX_REQUEST_BYTES: usize = 5 * 1024 * 1024;
pub const MAX_FRAME_BYTES: usize = 5 * 1024 * 1024;
pub const MAX_RESPONSE_HEADER_BYTES: usize = 16 * 1024;
pub const MAX_FRAMES_PER_EXCHANGE: usize = 1024;
pub const MAX_LIST_PAGES: usize = 100;
pub const MAX_TOOLS: usize = 1000;
pub const MAX_LIST_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_STDIO_PER_PROJECT: usize = 4;
pub const MAX_HTTP_PREPARATIONS: usize = 4;
pub const PREPARATION_TIMEOUT_MS: u64 = 30_000;
pub const MAX_RESTARTS_PER_RUN_SERVER: usize = 1;
pub const MAX_PUBLIC_NAME_BYTES: usize = 64;

/// Separate nominal identities: source edits, execution consent and tool definitions
/// must never substitute for one another. Hash inputs are domain-separated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalFingerprint(pub [u8; 32]);
impl ApprovalFingerprint {
    /// Caller supplies the canonical approval PROJECTION, never the whole config
    /// or resolved credentials. Projection policy is owned by approval.rs.
    pub fn from_projection(canonical: &[u8]) -> Self {
        let mut hash = Sha256::new();
        hash.update(b"evorch.mcp.approval.v1\0");
        hash.update(canonical);
        Self(hash.finalize().into())
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DefinitionFingerprint(pub [u8; 32]);
impl DefinitionFingerprint {
    pub fn for_definition(definition: &McpToolDefinition) -> Self {
        let mut hash = Sha256::new();
        hash.update(b"evorch.mcp.definition.v1\0");
        let value = serde_json::to_value(definition).expect("JSON definition is serializable");
        hash.update(canonical_json_bytes(&value));
        Self(hash.finalize().into())
    }
}
/// Stable object-key ordering independent of serde_json feature unification.
/// Arrays retain their order; callers choose the projection before canonicalizing.
pub fn canonical_json_bytes(value: &Value) -> Vec<u8> {
    fn append(value: &Value, out: &mut Vec<u8>) {
        match value {
            Value::Object(object) => {
                out.push(b'{');
                let mut fields: Vec<_> = object.iter().collect();
                fields.sort_unstable_by(|left, right| left.0.cmp(right.0));
                for (index, (key, value)) in fields.into_iter().enumerate() {
                    if index != 0 {
                        out.push(b',');
                    }
                    serde_json::to_writer(&mut *out, key).expect("JSON key");
                    out.push(b':');
                    append(value, out);
                }
                out.push(b'}');
            }
            Value::Array(array) => {
                out.push(b'[');
                for (index, item) in array.iter().enumerate() {
                    if index != 0 {
                        out.push(b',');
                    }
                    append(item, out);
                }
                out.push(b']');
            }
            _ => serde_json::to_writer(out, value).expect("JSON value"),
        }
    }
    let mut output = Vec::new();
    append(value, &mut output);
    output
}
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteToolKey {
    pub server_id: String,
    pub tool_name: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalScope {
    pub canonical_root: PathBuf,
    pub server_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SecretUnavailableReason {
    Unavailable,
    Locked,
    Denied,
    Missing,
}
/// Composition edge injects a KEYCHAIN-ONLY store (fake in tests), or an explicit
/// unavailable backend. tools/runtime never probe keyring or call open_default.
#[derive(Clone)]
pub enum McpCredentialBackend {
    Available(Arc<dyn CredentialStore>),
    Unavailable(SecretUnavailableReason),
}
impl fmt::Debug for McpCredentialBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Available(_) => f.write_str("McpCredentialBackend(<injected>)"),
            Self::Unavailable(reason) => f.debug_tuple("Unavailable").field(reason).finish(),
        }
    }
}
pub trait McpSecretResolver: Send + Sync {
    fn resolve(
        &self,
        scope: &ApprovalScope,
        source: &ValueSource,
        environment: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Secret, super::McpErrorKind>;
}
/// Not serializable, not forgeable by configuration/model/UI DTO deserialization.
/// Only approval.rs can mint it after checking ledger + current projection.
pub struct ApprovalPermit {
    pub(super) scope: ApprovalScope,
    pub(super) fingerprint: ApprovalFingerprint,
    pub(super) revoked: Arc<std::sync::atomic::AtomicBool>,
}
impl ApprovalPermit {
    pub fn scope(&self) -> &ApprovalScope {
        &self.scope
    }
    pub fn fingerprint(&self) -> ApprovalFingerprint {
        self.fingerprint
    }
    pub fn is_revoked(&self) -> bool {
        self.revoked.load(std::sync::atomic::Ordering::Acquire)
    }
}
impl fmt::Debug for ApprovalPermit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ApprovalPermit(<opaque>)")
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalPreviewId(pub [u8; 16]);
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpApprovalPreview {
    pub id: ApprovalPreviewId,
    pub scope: ApprovalScope,
    pub source_revision: SourceRevision,
    pub fingerprint: ApprovalFingerprint,
    /// Safe structured warnings, never URL/headers/args/environment values.
    pub warnings: Vec<ApprovalWarning>,
    pub previous_fingerprint: Option<ApprovalFingerprint>,
    pub summary: McpApprovalSummary,
    pub approved_ips: Vec<IpAddr>,
}
/// Safe diff/preview metadata: no argument, URL query or header values.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpApprovalSummary {
    pub executable: Option<PathBuf>,
    pub canonical_cwd: Option<PathBuf>,
    pub destination: Option<McpDestinationSummary>,
    pub environment_keys: Vec<String>,
    pub secret_references: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpDestinationSummary {
    pub scheme: String,
    pub host: String,
    pub port: u16,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum ApprovalWarning {
    HostProcessOutsideSandbox,
    PrivateDestination,
    PlaintextHttp,
    SendsSecretReferences,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfirmMcpApproval {
    pub preview_id: ApprovalPreviewId,
    pub scope: ApprovalScope,
    pub expected_revision: SourceRevision,
    pub expected_fingerprint: ApprovalFingerprint,
}
/// Fresh, safe inspection prepared by the service (B's canonical projection;
/// C's explicit checked destination inspection). Not a serialized permission.
#[derive(Debug, Clone)]
pub struct McpApprovalInspection {
    pub scope: ApprovalScope,
    pub source_revision: SourceRevision,
    pub fingerprint: ApprovalFingerprint,
    pub summary: McpApprovalSummary,
    pub warnings: Vec<ApprovalWarning>,
    pub approved_ips: Vec<IpAddr>,
}
/// preview/check are NOT execution and must not spawn/connect/resolve secrets.
/// DNS needed for destination preview is an explicit checked service operation.
pub trait McpApprovalStore: Send + Sync {
    fn preview(
        &self,
        inspection: &McpApprovalInspection,
    ) -> Result<McpApprovalPreview, super::McpErrorKind>;
    /// Service re-inspects current sources first; compare against BOTH pending
    /// preview identity and the fresh inspection, not the UI's cached metadata.
    fn approve(
        &self,
        confirmation: &ConfirmMcpApproval,
        current: &McpApprovalInspection,
    ) -> Result<(), super::McpErrorKind>;
    fn revoke(&self, scope: &ApprovalScope) -> Result<(), super::McpErrorKind>;
    /// Fresh inspection is revalidated immediately before launch/connection.
    fn check(&self, current: &McpApprovalInspection)
    -> Result<ApprovalPermit, super::McpErrorKind>;
}
/// Secret-bearing resolved values are ephemeral and never serialized or logged.
pub struct ResolvedMcpServer {
    pub scope: ApprovalScope,
    pub fingerprint: ApprovalFingerprint,
    pub transport: ResolvedMcpTransport,
    pub startup_timeout: Duration,
    pub request_timeout: Duration,
}
pub enum ResolvedMcpTransport {
    Stdio {
        executable: PathBuf,
        args: Vec<Secret>,
        cwd: PathBuf,
        env: BTreeMap<String, Secret>,
    },
    Http {
        url: Secret,
        headers: BTreeMap<String, Secret>,
        token: Option<Secret>,
        destination: crate::network_guard::mcp::ApprovedMcpDestination,
    },
}
impl fmt::Debug for ResolvedMcpServer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ResolvedMcpServer(<redacted>)")
    }
}
impl fmt::Debug for ResolvedMcpTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ResolvedMcpTransport(<redacted>)")
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrozenMcpTool {
    pub public_name: String,
    pub remote: RemoteToolKey,
    pub definition: McpToolDefinition,
    pub approval_fingerprint: ApprovalFingerprint,
    pub definition_fingerprint: DefinitionFingerprint,
}
/// Ordered once at preparation. Restore registers this without discovery.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrozenMcpSnapshot {
    pub tools: Vec<FrozenMcpTool>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum McpServerStatus {
    Disabled,
    Invalid,
    NeedsApproval,
    SecretUnavailable,
    Connecting,
    Ready,
    Unavailable,
    ProcessLimitReached,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpStatusEvent {
    pub scope: ApprovalScope,
    pub status: McpServerStatus,
    pub reason: Option<super::McpErrorKind>,
}
