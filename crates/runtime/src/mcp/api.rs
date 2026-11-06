use agents::Role;
use async_trait::async_trait;
use config::mcp::{McpOverrideMutation, ProjectMcpConfig, SourceRevision};
use sandbox::Secret;
use std::{path::PathBuf, sync::Arc};
use tokio::sync::watch;
use tools::mcp::{
    McpErrorKind,
    transport::{CancelContext, McpTransport, ShutdownReason},
    types::{
        ApprovalScope, ConfirmMcpApproval, FrozenMcpSnapshot, McpApprovalPreview,
        McpCredentialBackend, McpServerStatus, McpStatusEvent,
    },
};

/// Configuration save races are distinct from stale approval previews. Keep the
/// GUI boundary classified: never forward filesystem errors or source text.
pub fn mcp_config_error_kind(error: config::mcp::McpConfigError) -> McpErrorKind {
    use config::mcp::McpConfigError;
    match error {
        McpConfigError::Conflict => McpErrorKind::ConfigConflict,
        McpConfigError::LimitExceeded => McpErrorKind::LimitExceeded,
        McpConfigError::Unavailable => McpErrorKind::Unavailable,
        McpConfigError::Io
        | McpConfigError::Syntax
        | McpConfigError::Invalid
        | McpConfigError::InvalidTemplate
        | McpConfigError::MissingEnvironment
        | McpConfigError::Symlink => McpErrorKind::Configuration,
    }
}

/// Ephemeral run inputs, never serialized into restore descriptors.
#[derive(Debug, Clone)]
pub struct PrepareMcpRun {
    pub canonical_root: PathBuf,
    pub run_id: String,
    pub role: Role,
    pub cancel: CancelContext,
}
#[derive(Debug, Clone)]
pub struct RestoreMcpRun {
    pub context: PrepareMcpRun,
    pub snapshot: FrozenMcpSnapshot,
}
/// Owns sessions (and their process permits) for exactly one run. Snapshot is
/// immutable even after config edits, revocation, cancellation or listChanged.
pub struct RunMcpLease {
    snapshot: Arc<FrozenMcpSnapshot>,
    registry: Option<Arc<tools::mcp::McpToolRegistry>>,
    cancel: Option<CancelContext>,
    statuses: Vec<McpStatusEvent>,
    sessions: Vec<Arc<dyn McpTransport>>,
}
impl RunMcpLease {
    /// Pure restore: no loader, credential lookup, permit, process or connection.
    pub fn restore_snapshot(snapshot: FrozenMcpSnapshot) -> Self {
        Self {
            snapshot: Arc::new(snapshot),
            registry: None,
            cancel: None,
            statuses: Vec::new(),
            sessions: Vec::new(),
        }
    }
    /// Pure host integration for a registry whose frozen metadata is already
    /// known. Lazy connectors still enforce live approval before first execution.
    pub fn from_registry(registry: Arc<tools::mcp::McpToolRegistry>) -> Result<Self, McpErrorKind> {
        let snapshot = registry
            .snapshot()
            .ok_or(McpErrorKind::Configuration)?
            .clone();
        let mut lease = Self::restore_snapshot(snapshot);
        lease.registry = Some(registry);
        Ok(lease)
    }
    pub(super) fn prepared(
        snapshot: FrozenMcpSnapshot,
        statuses: Vec<McpStatusEvent>,
        sessions: Vec<Arc<dyn McpTransport>>,
        registry: Arc<tools::mcp::McpToolRegistry>,
        cancel: CancelContext,
    ) -> Self {
        Self {
            snapshot: Arc::new(snapshot),
            statuses,
            sessions,
            registry: Some(registry),
            cancel: Some(cancel),
        }
    }
    /// Inert, atomic registration into a run-local executor. Never a host-tools
    /// inheritance entry and never a mutation of another run's base executor.
    pub fn register_executor(
        &self,
        base: &tools::ToolExecutor,
        bus: Arc<event_bus::EventBus>,
    ) -> Result<tools::ToolExecutor, McpErrorKind> {
        let tools = self
            .registry
            .as_ref()
            .map(|registry| registry.tools())
            .transpose()
            .map_err(|error| error.kind)?
            .unwrap_or_default();
        base.fork_with_run_tools(
            tools
                .into_iter()
                .map(|tool| Arc::new(tool.with_event_bus(bus.clone())) as Arc<dyn tools::Tool>),
        )
        .map_err(|_| McpErrorKind::Configuration)
    }
    pub fn snapshot(&self) -> &FrozenMcpSnapshot {
        &self.snapshot
    }
    pub fn statuses(&self) -> &[McpStatusEvent] {
        &self.statuses
    }
    /// Each transport also provides Drop cleanup if owner cancellation aborts this.
    pub async fn shutdown(&self, reason: ShutdownReason) {
        for session in &self.sessions {
            let _ = session.shutdown(reason).await;
        }
    }
}
impl Drop for RunMcpLease {
    fn drop(&mut self) {
        if let Some(cancel) = &self.cancel {
            cancel.cancel();
        }
    }
}
#[async_trait]
pub trait McpRunService: Send + Sync {
    /// Exclude disabled/other-role/unapproved servers before secret resolution,
    /// admission, discovery or connection. A failing server does not fail the run.
    async fn prepare_new_run(&self, request: PrepareMcpRun) -> Result<RunMcpLease, McpErrorKind>;
    /// Synchronous, inert registration of saved snapshot; first call validates live
    /// consent/destination/definition. Existing conversation prefix never changes.
    fn restore_snapshot(&self, request: RestoreMcpRun) -> Result<RunMcpLease, McpErrorKind>;
}
#[derive(Debug, Clone)]
pub struct McpSettingsView {
    pub config: ProjectMcpConfig,
    pub statuses: Vec<McpStatusEvent>,
}
/// Secret is deliberately not Serialize. Never store this command in model/event
/// history. Keychain write precedes TOML save; rollback only newly-created keys.
#[derive(Debug)]
pub struct SaveMcpSecret {
    pub scope: ApprovalScope,
    pub logical_name: String,
    pub value: Secret,
    pub expected_revision: SourceRevision,
    pub mutation: McpOverrideMutation,
}
/// Privileged GUI-only API. Not an agent tool, not model ask_user approval.
/// Call on a background executor, never synchronously on the render thread.
#[async_trait]
pub trait McpGuiService: Send + Sync {
    async fn settings(&self, canonical_root: PathBuf) -> Result<McpSettingsView, McpErrorKind>;
    async fn save_override(
        &self,
        canonical_root: PathBuf,
        expected_revision: SourceRevision,
        mutation: McpOverrideMutation,
    ) -> Result<McpSettingsView, McpErrorKind>;
    /// Last complete bounded state includes notifications/approval entry points.
    fn subscribe_status(&self, canonical_root: PathBuf) -> watch::Receiver<Vec<McpStatusEvent>>;
    async fn preview_approval(
        &self,
        scope: ApprovalScope,
        expected_revision: SourceRevision,
    ) -> Result<McpApprovalPreview, McpErrorKind>;
    /// Re-read + compare preview ID, revision and fingerprint before persisting.
    async fn confirm_approval(&self, confirmation: ConfirmMcpApproval) -> Result<(), McpErrorKind>;
    async fn revoke_approval(&self, scope: ApprovalScope) -> Result<(), McpErrorKind>;
    /// Same approval path and project limiter as runs. No approval bypass.
    async fn test_connection(
        &self,
        scope: ApprovalScope,
        expected_revision: SourceRevision,
        cancel: CancelContext,
    ) -> Result<McpServerStatus, McpErrorKind>;
    async fn save_secret(&self, request: SaveMcpSecret) -> Result<McpSettingsView, McpErrorKind>;
}
/// Fail-closed fallback for disabled hosts or failed local service composition.
/// Records injection without probing backends or doing I/O; benchmark never calls it.
pub struct UnavailableMcpService {
    credentials: McpCredentialBackend,
    statuses: watch::Sender<Vec<McpStatusEvent>>,
}
impl UnavailableMcpService {
    pub fn new(credentials: McpCredentialBackend) -> Self {
        Self {
            credentials,
            statuses: watch::channel(Vec::new()).0,
        }
    }
    pub fn credentials(&self) -> &McpCredentialBackend {
        &self.credentials
    }
}
#[async_trait]
impl McpRunService for UnavailableMcpService {
    async fn prepare_new_run(&self, _request: PrepareMcpRun) -> Result<RunMcpLease, McpErrorKind> {
        Err(McpErrorKind::Unavailable)
    }
    fn restore_snapshot(&self, request: RestoreMcpRun) -> Result<RunMcpLease, McpErrorKind> {
        Ok(RunMcpLease::restore_snapshot(request.snapshot))
    }
}
#[async_trait]
impl McpGuiService for UnavailableMcpService {
    async fn settings(&self, _root: PathBuf) -> Result<McpSettingsView, McpErrorKind> {
        Err(McpErrorKind::Unavailable)
    }
    async fn save_override(
        &self,
        _root: PathBuf,
        _revision: SourceRevision,
        _mutation: McpOverrideMutation,
    ) -> Result<McpSettingsView, McpErrorKind> {
        Err(McpErrorKind::Unavailable)
    }
    fn subscribe_status(&self, _canonical_root: PathBuf) -> watch::Receiver<Vec<McpStatusEvent>> {
        self.statuses.subscribe()
    }
    async fn preview_approval(
        &self,
        _scope: ApprovalScope,
        _revision: SourceRevision,
    ) -> Result<McpApprovalPreview, McpErrorKind> {
        Err(McpErrorKind::Unavailable)
    }
    async fn confirm_approval(
        &self,
        _confirmation: ConfirmMcpApproval,
    ) -> Result<(), McpErrorKind> {
        Err(McpErrorKind::Unavailable)
    }
    async fn revoke_approval(&self, _scope: ApprovalScope) -> Result<(), McpErrorKind> {
        Err(McpErrorKind::Unavailable)
    }
    async fn test_connection(
        &self,
        _scope: ApprovalScope,
        _revision: SourceRevision,
        _cancel: CancelContext,
    ) -> Result<McpServerStatus, McpErrorKind> {
        Err(McpErrorKind::NeedsApproval)
    }
    async fn save_secret(&self, _request: SaveMcpSecret) -> Result<McpSettingsView, McpErrorKind> {
        Err(McpErrorKind::Unavailable)
    }
}
