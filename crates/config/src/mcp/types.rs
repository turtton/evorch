use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, fmt, path::PathBuf};
pub const MAX_CONFIG_BYTES: usize = 1024 * 1024;
pub const MAX_SERVERS: usize = 64;
pub const MAX_STRING_BYTES: usize = 64 * 1024;
pub const MIN_TIMEOUT_MS: u64 = 1;
pub const MAX_TIMEOUT_MS: u64 = 600_000;
pub const DEFAULT_STARTUP_TIMEOUT_MS: u64 = 10_000;
pub const DEFAULT_REQUEST_TIMEOUT_MS: u64 = 60_000;
/// Mirrors agents::Role::config_key; runtime tests must check this leaf-crate list.
pub const MCP_ROLE_KEYS: &[&str] = &[
    "orchestrator",
    "explorer",
    "worker",
    "reviewer",
    "web_researcher",
    "planner",
    "advisor",
    "multimodal_looker",
    "evaluator",
];
/// Optimistic edit identity of BOTH original files, not execution authorization.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceRevision(pub [u8; 32]);
impl SourceRevision {
    pub fn from_sources(imported: Option<&[u8]>, overrides: Option<&[u8]>) -> Self {
        let mut hash = Sha256::new();
        hash.update(b"evorch.mcp.source.v1\0");
        for source in [imported, overrides] {
            hash.update([u8::from(source.is_some())]);
            if let Some(bytes) = source {
                hash.update((bytes.len() as u64).to_be_bytes());
                hash.update(bytes);
            }
        }
        Self(hash.finalize().into())
    }
}
/// AST is retained through normalization; defaults are literal and never re-expanded.
/// Debug is deliberately opaque: literal configuration may contain credentials.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Template(pub Vec<TemplatePart>);
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TemplatePart {
    Literal {
        value: String,
    },
    Env {
        name: String,
        default: Option<String>,
    },
}
impl fmt::Debug for Template {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Template(<redacted>)")
    }
}
impl fmt::Debug for TemplatePart {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("TemplatePart(<redacted>)")
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ValueSource {
    Template { template: Template },
    Keychain { name: String },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum McpTransportDefinition {
    Stdio {
        command: Template,
        args: Vec<Template>,
        cwd: Option<Template>,
        env: BTreeMap<String, ValueSource>,
    },
    Http {
        url: Template,
        headers: BTreeMap<String, ValueSource>,
        token: Option<ValueSource>,
    },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpServerDefinition {
    pub enabled: bool,
    /// Empty means unpublished; omission in source is normalized to ["worker"].
    pub roles: Vec<String>,
    pub transport: McpTransportDefinition,
    pub startup_timeout_ms: u64,
    pub request_timeout_ms: u64,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum McpSource {
    ImportedJson,
    ProjectToml,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServerEntry {
    pub source: McpSource,
    pub imported_exists: bool,
    /// None for invalid entries and tombstones; diagnostics distinguish them.
    pub definition: Option<McpServerDefinition>,
    pub tombstone: bool,
    pub diagnostics: Vec<McpConfigDiagnostic>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectMcpConfig {
    pub canonical_root: PathBuf,
    pub revision: SourceRevision,
    pub servers: BTreeMap<String, McpServerEntry>,
    /// A broken TOML document disables all NEW preparation; never fall back to JSON.
    pub preparation_blocked: bool,
    pub diagnostics: Vec<McpConfigDiagnostic>,
}
/// No raw parser error, offending value, URL, header or file contents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpConfigDiagnostic {
    pub server_id: Option<String>,
    pub source: McpSource,
    pub code: McpConfigError,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum McpConfigError {
    #[error("MCP configuration implementation unavailable")]
    Unavailable,
    #[error("MCP configuration I/O failed")]
    Io,
    #[error("MCP configuration syntax invalid")]
    Syntax,
    #[error("MCP configuration validation failed")]
    Invalid,
    #[error("MCP template syntax invalid")]
    InvalidTemplate,
    #[error("MCP environment reference unavailable")]
    MissingEnvironment,
    #[error("MCP configuration limit exceeded")]
    LimitExceeded,
    #[error("MCP source changed since preview")]
    Conflict,
    #[error("MCP configuration symlink refused")]
    Symlink,
}
#[derive(Debug, Clone)]
pub enum McpOverrideMutation {
    /// Full replacement, never a field merge. Serializer writes source syntax, not AST.
    Upsert {
        server_id: String,
        definition: McpServerDefinition,
    },
    Disable {
        server_id: String,
    },
    RemoveOverride {
        server_id: String,
    },
}
