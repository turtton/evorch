//! Approved, run-local MCP preparation. No constructor starts or contacts a server.
mod gui;
use super::{
    McpRunService, PrepareMcpRun, ProjectMcpLimiter, RestoreMcpRun, RunMcpLease,
    mcp_config_error_kind,
};
use agents::Role;
use async_trait::async_trait;
use config::mcp::{
    self, McpServerDefinition, McpTransportDefinition, SourceRevision, Template, ValueSource,
};
use sandbox::Secret;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::{Mutex as AsyncMutex, watch};
use tools::{
    NetworkGuard,
    mcp::{
        McpClient, McpClientInfo, McpError, McpErrorKind, McpRegistryConnector, McpRegistrySession,
        McpServerTools, McpToolDefinition, McpToolRegistry, McpToolResult, McpToolTransport,
        McpValueField,
        approval::{inspect_http, inspect_stdio},
        secrets::SecretResolver,
        transport::{
            CancelContext, McpTransport, McpTransportFactory, ShutdownReason, TransportKind,
            http::HttpTransportFactory, stdio::StdioTransport,
        },
        types::{
            ApprovalPermit, ApprovalScope, FrozenMcpSnapshot, FrozenMcpTool, McpApprovalInspection,
            McpApprovalStore, McpCredentialBackend, McpServerStatus, McpStatusEvent,
            PREPARATION_TIMEOUT_MS, ResolvedMcpServer, ResolvedMcpTransport,
            SecretUnavailableReason,
        },
    },
};

/// One instance is shared by all project runs and GUI connection tests. Backend
/// injection is feature-independent; neither tools nor runtime opens a keychain.
#[derive(Clone)]
pub struct ProjectMcpService {
    inner: Arc<ServiceInner>,
}
struct ServiceInner {
    approvals: Arc<dyn McpApprovalStore>,
    secrets: SecretResolver,
    credentials: McpCredentialBackend,
    secret_write: Mutex<()>,
    guard: Arc<NetworkGuard>,
    limiter: Arc<ProjectMcpLimiter>,
    stdio: Option<Arc<dyn McpTransportFactory>>,
    http: Arc<dyn McpTransportFactory>,
    statuses: Mutex<BTreeMap<PathBuf, watch::Sender<Vec<McpStatusEvent>>>>,
}
impl ProjectMcpService {
    pub fn new(
        backend: McpCredentialBackend,
        approvals: Arc<dyn McpApprovalStore>,
        guard: Arc<NetworkGuard>,
    ) -> Self {
        Self {
            inner: Arc::new(ServiceInner {
                approvals,
                secrets: SecretResolver::new(backend.clone()),
                credentials: backend,
                secret_write: Mutex::new(()),
                http: Arc::new(HttpTransportFactory::new(guard.clone())),
                stdio: None,
                guard,
                limiter: Arc::new(ProjectMcpLimiter::default()),
                statuses: Mutex::new(BTreeMap::new()),
            }),
        }
    }
    /// Inert host/test injection, not an agent-facing approval mechanism.
    pub fn with_transport_factories(
        mut self,
        stdio: Arc<dyn McpTransportFactory>,
        http: Arc<dyn McpTransportFactory>,
    ) -> Self {
        let inner = Arc::get_mut(&mut self.inner).expect("configure MCP service before sharing it");
        inner.stdio = Some(stdio);
        inner.http = http;
        self
    }
    pub fn limiter(&self) -> Arc<ProjectMcpLimiter> {
        self.inner.limiter.clone()
    }
    pub fn subscribe_status(&self, root: PathBuf) -> watch::Receiver<Vec<McpStatusEvent>> {
        self.inner
            .statuses
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(root)
            .or_insert_with(|| watch::channel(Vec::new()).0)
            .subscribe()
    }
}
impl ServiceInner {
    fn publish(&self, event: McpStatusEvent) {
        let mut projects = self
            .statuses
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let sender = projects
            .entry(event.scope.canonical_root.clone())
            .or_insert_with(|| watch::channel(Vec::new()).0);
        sender.send_modify(|events| {
            events.retain(|old| old.scope.server_id != event.scope.server_id);
            events.push(event);
            events.sort_by(|a, b| a.scope.server_id.cmp(&b.scope.server_id));
        });
    }
    fn value(
        &self,
        scope: &ApprovalScope,
        source: &ValueSource,
        field: McpValueField,
    ) -> Result<Secret, McpErrorKind> {
        match source {
            ValueSource::Template { template } => Ok(Secret::from(expand(template, field)?)),
            ValueSource::Keychain { .. } => self
                .secrets
                .resolve_classified(scope, source, &process_env)
                .map_err(secret_error),
        }
    }
    /// Performs local projection/check first. DNS is allowed only after the
    /// offline consent check; credentials are not read until destination validation.
    async fn authorize(
        &self,
        scope: &ApprovalScope,
        revision: SourceRevision,
        definition: &McpServerDefinition,
    ) -> Result<(McpApprovalInspection, ApprovalPermit, LaunchTarget), McpErrorKind> {
        match &definition.transport {
            McpTransportDefinition::Stdio { command, cwd, .. } => {
                let cwd = cwd
                    .as_ref()
                    .map(|v| expand(v, McpValueField::Cwd))
                    .transpose()?
                    .unwrap_or_else(|| ".".into());
                let cwd = std::fs::canonicalize(scope.canonical_root.join(cwd))
                    .map_err(|_| McpErrorKind::Unavailable)?;
                if !cwd.is_dir() {
                    return Err(McpErrorKind::Configuration);
                }
                let executable = executable(&expand(command, McpValueField::Command)?, &cwd)?;
                let inspection =
                    inspect_stdio(scope.clone(), revision, definition, &executable, &cwd)?;
                let permit = self.approvals.check(&inspection)?;
                Ok((inspection, permit, LaunchTarget::Stdio { executable, cwd }))
            }
            McpTransportDefinition::Http { url, .. } => {
                let url = expand(url, McpValueField::Url)?;
                mcp::validate::validate_url(&url).map_err(mcp_config_error_kind)?;
                let parsed = url::Url::parse(&url).map_err(|_| McpErrorKind::Configuration)?;
                let saved_ips = self.approvals.approved_ips(scope)?;
                let offline = inspect_http(
                    scope.clone(),
                    revision,
                    definition,
                    &parsed,
                    saved_ips.clone(),
                )?;
                let _offline_permit = self.approvals.check(&offline)?;
                let checked = self
                    .guard
                    .inspect_mcp_destination(&url)
                    .await
                    .map_err(|_| McpErrorKind::Transport)?;
                if !saved_ips.is_empty() && checked.ips.iter().any(|ip| !saved_ips.contains(ip)) {
                    return Err(McpErrorKind::NeedsApproval);
                }
                let ips = if checked.requires_approval {
                    checked.ips
                } else {
                    Vec::new()
                };
                let inspection =
                    inspect_http(scope.clone(), revision, definition, &parsed, ips.clone())?;
                let permit = self.approvals.check(&inspection)?;
                let destination = self
                    .guard
                    .approve_mcp_destination(&url, &permit, &ips)
                    .await
                    .map_err(|_| McpErrorKind::NeedsApproval)?;
                Ok((inspection, permit, LaunchTarget::Http { url, destination }))
            }
        }
    }
    async fn resolved(
        &self,
        scope: &ApprovalScope,
        revision: SourceRevision,
        definition: &McpServerDefinition,
        expected_fingerprint: Option<tools::mcp::types::ApprovalFingerprint>,
    ) -> Result<(ResolvedMcpServer, ApprovalPermit), McpErrorKind> {
        let (inspection, permit, target) = self.authorize(scope, revision, definition).await?;
        // Restored bindings must match before secrets are resolved or I/O starts.
        if expected_fingerprint.is_some_and(|expected| expected != inspection.fingerprint) {
            return Err(McpErrorKind::ApprovalRevoked);
        }
        let transport = match (&definition.transport, target) {
            (
                McpTransportDefinition::Stdio { args, env, .. },
                LaunchTarget::Stdio { executable, cwd },
            ) => {
                let args = args
                    .iter()
                    .map(|v| expand(v, McpValueField::Args).map(Secret::from))
                    .collect::<Result<Vec<_>, _>>()?;
                let env = env
                    .iter()
                    .map(|(key, v)| {
                        self.value(scope, v, McpValueField::Env)
                            .map(|v| (key.clone(), v))
                    })
                    .collect::<Result<BTreeMap<_, _>, _>>()?;
                if args
                    .iter()
                    .chain(env.values())
                    .any(|v| v.expose().contains('\0'))
                {
                    return Err(McpErrorKind::Configuration);
                }
                ResolvedMcpTransport::Stdio {
                    executable,
                    args,
                    cwd,
                    env,
                }
            }
            (
                McpTransportDefinition::Http { headers, token, .. },
                LaunchTarget::Http { url, destination },
            ) => {
                let headers = headers
                    .iter()
                    .map(|(key, v)| {
                        self.value(scope, v, McpValueField::Headers).and_then(|v| {
                            mcp::validate::validate_header_value(v.expose())
                                .map_err(mcp_config_error_kind)?;
                            Ok((key.clone(), v))
                        })
                    })
                    .collect::<Result<BTreeMap<_, _>, _>>()?;
                let token = token
                    .as_ref()
                    .map(|v| self.value(scope, v, McpValueField::Token))
                    .transpose()?;
                if let Some(value) = &token {
                    mcp::validate::validate_header_value(value.expose())
                        .map_err(mcp_config_error_kind)?;
                }
                ResolvedMcpTransport::Http {
                    url: Secret::from(url),
                    headers,
                    token,
                    destination,
                }
            }
            _ => return Err(McpErrorKind::Configuration),
        };
        Ok((
            ResolvedMcpServer {
                scope: scope.clone(),
                fingerprint: inspection.fingerprint,
                transport,
                startup_timeout: Duration::from_millis(definition.startup_timeout_ms),
                request_timeout: Duration::from_millis(definition.request_timeout_ms),
            },
            permit,
        ))
    }
    async fn prepare_server(
        &self,
        scope: &ApprovalScope,
        revision: SourceRevision,
        definition: &McpServerDefinition,
        run_cancel: &CancelContext,
        expected_fingerprint: Option<tools::mcp::types::ApprovalFingerprint>,
        deadline: Option<tokio::time::Instant>,
    ) -> Result<Arc<PreparedSession>, McpErrorKind> {
        // One budget covers resolution, connection, negotiation and all list pages.
        let deadline = deadline.unwrap_or_else(|| {
            tokio::time::Instant::now() + Duration::from_millis(PREPARATION_TIMEOUT_MS)
        });
        let (resolved, permit) = tokio::select! {
            biased;
            _=run_cancel.cancelled()=>return Err(McpErrorKind::Cancelled),
            result=tokio::time::timeout_at(deadline,self.resolved(scope,revision,definition,expected_fingerprint))=>result.unwrap_or(Err(McpErrorKind::Timeout))?,
        };
        let kind = match resolved.transport {
            ResolvedMcpTransport::Stdio { .. } => TransportKind::Stdio,
            ResolvedMcpTransport::Http { .. } => TransportKind::Http,
        };
        let _http_admission = if kind == TransportKind::Http {
            Some(self.limiter.try_http_preparation(&scope.canonical_root)?)
        } else {
            None
        };
        let fingerprint = resolved.fingerprint;
        let startup = resolved.startup_timeout;
        let request_timeout = resolved.request_timeout;
        let owner = SessionOwner::new(run_cancel.clone());
        self.publish(McpStatusEvent {
            scope: scope.clone(),
            status: McpServerStatus::Connecting,
            reason: None,
        });
        let connect = async {
            let pair: (Arc<dyn McpTransport>, Option<Arc<StdioTransport>>) =
                if kind == TransportKind::Stdio && self.stdio.is_none() {
                    let transport = StdioTransport::connect(
                        resolved,
                        permit,
                        self.limiter.clone(),
                        owner.cancel.clone(),
                    )
                    .await
                    .map_err(|error| error.kind)?;
                    (transport.clone(), Some(transport))
                } else {
                    let factory = if kind == TransportKind::Stdio {
                        self.stdio.as_ref().expect("injected factory")
                    } else {
                        &self.http
                    };
                    (
                        factory
                            .connect(resolved, permit, self.limiter.clone(), owner.cancel.clone())
                            .await
                            .map_err(|error| error.kind)?,
                        None,
                    )
                };
            Ok::<_, McpErrorKind>(pair)
        };
        let (raw, native_stdio) = tokio::select! {
            biased;
            _=run_cancel.cancelled()=>return Err(McpErrorKind::Cancelled),
            result=tokio::time::timeout_at(deadline,connect)=>result.unwrap_or(Err(McpErrorKind::Timeout))?,
        };
        let initialize = async {
            let mut client = McpClient::connect_transport_with_timeouts(
                raw.clone(),
                scope.server_id.clone(),
                kind,
                McpClientInfo {
                    name: "evorch".into(),
                    version: env!("CARGO_PKG_VERSION").into(),
                },
                startup,
                request_timeout,
                owner.cancel.clone(),
            )
            .await
            .map_err(|error| error.kind)?;
            let definitions = client.list_tools().await.map_err(|error| error.kind)?;
            Ok::<_, McpErrorKind>((client, definitions))
        };
        let result = tokio::select! {
            biased;
            _=run_cancel.cancelled()=>Err(McpErrorKind::Cancelled),
            result=tokio::time::timeout_at(deadline,initialize)=>result.unwrap_or(Err(McpErrorKind::Timeout)),
        };
        let (client, definitions) = match result {
            Ok(prepared) => prepared,
            Err(kind) => {
                // Once a transport exists, cancellation/deadline must await cleanup,
                // not merely drop the initialization future and hope Drop can reap.
                let _ = raw
                    .shutdown(if kind == McpErrorKind::Cancelled {
                        ShutdownReason::Cancelled
                    } else {
                        ShutdownReason::InitializationFailed
                    })
                    .await;
                return Err(kind);
            }
        };
        Ok(Arc::new(PreparedSession {
            client: AsyncMutex::new(ClientState {
                client,
                failed: false,
                restarted: false,
            }),
            native_stdio,
            kind,
            label: scope.server_id.clone(),
            startup,
            request_timeout,
            definitions,
            fingerprint,
            limiter: self.limiter.clone(),
            root: scope.canonical_root.clone(),
            raw,
            _owner: owner,
        }))
    }
}
enum LaunchTarget {
    Stdio {
        executable: PathBuf,
        cwd: PathBuf,
    },
    Http {
        url: String,
        destination: tools::network_guard::mcp::ApprovedMcpDestination,
    },
}
struct SessionOwner {
    cancel: CancelContext,
    link: tokio::task::JoinHandle<()>,
}
impl SessionOwner {
    fn new(parent: CancelContext) -> Self {
        let cancel = CancelContext::default();
        let child = cancel.clone();
        let link = tokio::spawn(async move {
            parent.cancelled().await;
            child.cancel();
        });
        Self { cancel, link }
    }
}
impl Drop for SessionOwner {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.link.abort();
    }
}
struct PreparedSession {
    limiter: Arc<ProjectMcpLimiter>,
    root: PathBuf,
    fingerprint: tools::mcp::types::ApprovalFingerprint,
    client: AsyncMutex<ClientState>,
    native_stdio: Option<Arc<StdioTransport>>,
    kind: TransportKind,
    label: String,
    startup: Duration,
    request_timeout: Duration,
    definitions: Vec<McpToolDefinition>,
    raw: Arc<dyn McpTransport>,
    _owner: SessionOwner,
}
struct ClientState {
    client: McpClient,
    failed: bool,
    restarted: bool,
}
#[async_trait]
impl McpRegistrySession for PreparedSession {
    fn definitions(&self) -> &[McpToolDefinition] {
        &self.definitions
    }
    async fn call_tool(
        &self,
        name: &str,
        args: serde_json::Value,
    ) -> Result<(McpToolResult, serde_json::Value), McpError> {
        let mut state = self.client.lock().await;
        if state.failed {
            let error = |kind| McpError {
                server: self.label.clone(),
                method: "tools/call",
                request_id: None,
                kind,
            };
            let modern_http = self.kind == TransportKind::Http
                && state.client.era() == tools::mcp::protocol::ProtocolEra::Modern;
            let _http_admission = if self.kind == TransportKind::Http {
                Some(
                    self.limiter
                        .try_http_preparation(&self.root)
                        .map_err(error)?,
                )
            } else {
                None
            };
            let recovery = async {
                let live = if modern_http {
                    // Modern HTTP is stateless. This is a later, DIFFERENT explicit
                    // call: re-list before it, never replay the unknown call.
                    state.client.list_tools().await?
                } else {
                    if state.restarted {
                        return Err(error(McpErrorKind::Unavailable));
                    }
                    state.restarted = true;
                    if self.kind == TransportKind::Stdio {
                        let Some(transport) = self.native_stdio.as_ref() else {
                            return Err(error(McpErrorKind::Unavailable));
                        };
                        transport.restart(self._owner.cancel.clone()).await?;
                    }
                    // Legacy HTTP may initialize a replacement session ONCE. A
                    // stdio process generation likewise gets one new negotiation.
                    let mut client = McpClient::connect_transport_with_timeouts(
                        self.raw.clone(),
                        self.label.clone(),
                        self.kind,
                        McpClientInfo {
                            name: "evorch".into(),
                            version: env!("CARGO_PKG_VERSION").into(),
                        },
                        self.startup,
                        self.request_timeout,
                        self._owner.cancel.clone(),
                    )
                    .await?;
                    if self.kind == TransportKind::Http && client.era() != state.client.era() {
                        return Err(error(McpErrorKind::Protocol));
                    }
                    let live = client.list_tools().await?;
                    state.client = client;
                    live
                };
                Ok(live)
            };
            let live = tokio::select! {
                biased;
                _=self._owner.cancel.cancelled()=>return Err(error(McpErrorKind::Cancelled)),
                result=tokio::time::timeout(Duration::from_millis(PREPARATION_TIMEOUT_MS),recovery)=>result.map_err(|_|error(McpErrorKind::Timeout))??,
            };
            if self
                .definitions
                .iter()
                .any(|saved| !live.iter().any(|definition| definition == saved))
            {
                return Err(error(McpErrorKind::DefinitionChanged));
            }
            state.failed = false;
        }
        match state.client.call_tool(name, args).await {
            Ok(result) => Ok((result, state.client.call_success_detail())),
            Err(error) => {
                state.failed = matches!(
                    error.kind,
                    McpErrorKind::Transport | McpErrorKind::OutcomeUnknown | McpErrorKind::Timeout
                );
                Err(error)
            }
        }
    }
}
struct RunConnector {
    inner: Arc<ServiceInner>,
    root: PathBuf,
    role: Role,
    sessions: AsyncMutex<BTreeMap<String, Arc<PreparedSession>>>,
    cancel: CancelContext,
    transports: BTreeMap<String, McpToolTransport>,
}
#[async_trait]
impl McpRegistryConnector for RunConnector {
    fn tool_transport(&self, id: &str) -> McpToolTransport {
        self.transports
            .get(id)
            .copied()
            .unwrap_or(McpToolTransport::Stdio)
    }
    async fn check_execution(&self, tool: &FrozenMcpTool) -> Result<(), McpError> {
        let check = async {
            if self.cancel.is_cancelled() {
                return Err(McpErrorKind::Cancelled);
            }
            let config = mcp::load_project(&self.root).map_err(mcp_config_error_kind)?;
            if config.preparation_blocked || config.canonical_root != self.root {
                return Err(McpErrorKind::Configuration);
            }
            let definition = config
                .servers
                .get(&tool.remote.server_id)
                .and_then(|entry| entry.definition.as_ref())
                .ok_or(McpErrorKind::ApprovalRevoked)?;
            let kind = match definition.transport {
                McpTransportDefinition::Stdio { .. } => McpToolTransport::Stdio,
                McpTransportDefinition::Http { .. } => McpToolTransport::Http,
            };
            if kind != tool.transport || !definition.enabled {
                return Err(McpErrorKind::ApprovalRevoked);
            }
            if !definition
                .roles
                .iter()
                .any(|role| role == self.role.config_key())
            {
                return Err(McpErrorKind::RoleNotAllowed);
            }
            let scope = ApprovalScope {
                canonical_root: self.root.clone(),
                server_id: tool.remote.server_id.clone(),
            };
            let (inspection, _, _) = self
                .inner
                .authorize(&scope, config.revision, definition)
                .await?;
            if inspection.fingerprint != tool.approval_fingerprint {
                return Err(McpErrorKind::ApprovalRevoked);
            }
            Ok(())
        }
        .await;
        check.map_err(|kind| McpError {
            server: tool.remote.server_id.clone(),
            method: "tools/call",
            request_id: None,
            kind,
        })
    }
    async fn connect(&self, tool: &FrozenMcpTool) -> Result<Arc<dyn McpRegistrySession>, McpError> {
        let error = |kind| McpError {
            server: tool.remote.server_id.clone(),
            method: "tools/call",
            request_id: None,
            kind,
        };
        let mut sessions = self.sessions.lock().await;
        if self.cancel.is_cancelled() {
            return Err(error(McpErrorKind::Cancelled));
        }
        if let Some(session) = sessions.get(&tool.remote.server_id) {
            return Ok(session.clone());
        }
        // Registration was inert. Recheck before the first lazy preparation too.
        self.check_execution(tool).await?;
        let config = mcp::load_project(&self.root).map_err(|e| error(mcp_config_error_kind(e)))?;
        let definition = config
            .servers
            .get(&tool.remote.server_id)
            .and_then(|entry| entry.definition.as_ref())
            .ok_or_else(|| error(McpErrorKind::ApprovalRevoked))?;
        if !definition.enabled {
            return Err(error(McpErrorKind::ApprovalRevoked));
        }
        if !definition
            .roles
            .iter()
            .any(|role| role == self.role.config_key())
        {
            return Err(error(McpErrorKind::RoleNotAllowed));
        }
        let scope = ApprovalScope {
            canonical_root: self.root.clone(),
            server_id: tool.remote.server_id.clone(),
        };
        let session = self
            .inner
            .prepare_server(
                &scope,
                config.revision,
                definition,
                &self.cancel,
                Some(tool.approval_fingerprint),
                None,
            )
            .await
            .map_err(error)?;
        if session.fingerprint != tool.approval_fingerprint {
            let _ = session
                .raw
                .shutdown(ShutdownReason::InitializationFailed)
                .await;
            return Err(error(McpErrorKind::ApprovalRevoked));
        }
        sessions.insert(tool.remote.server_id.clone(), session.clone());
        Ok(session)
    }
    async fn shutdown(&self, reason: ShutdownReason) {
        self.cancel.cancel();
        let raw: Vec<_> = self
            .sessions
            .lock()
            .await
            .values()
            .map(|session| session.raw.clone())
            .collect();
        for transport in raw {
            let _ = transport.shutdown(reason).await;
        }
    }
}
#[async_trait]
impl McpRunService for ProjectMcpService {
    async fn prepare_new_run(
        &self,
        mut request: PrepareMcpRun,
    ) -> Result<RunMcpLease, McpErrorKind> {
        let config = mcp::load_project(&request.canonical_root).map_err(mcp_config_error_kind)?;
        request.canonical_root = config.canonical_root.clone();
        if config.preparation_blocked {
            return Err(McpErrorKind::Configuration);
        }
        if let Ok(mut statuses) = self.inner.statuses.lock()
            && let Some(sender) = statuses.get_mut(&config.canonical_root)
        {
            sender.send_modify(|events| {
                events.retain(|event| config.servers.contains_key(&event.scope.server_id))
            });
        }
        let mut statuses = Vec::new();
        let mut servers = Vec::new();
        let mut sessions = BTreeMap::new();
        let mut transports = BTreeMap::new();
        let deadline = tokio::time::Instant::now() + Duration::from_millis(PREPARATION_TIMEOUT_MS);
        // Sequential admission is deterministic by server ID and also bounds HTTP
        // discovery concurrency (one); the project process limiter never waits.
        for (id, entry) in &config.servers {
            let scope = ApprovalScope {
                canonical_root: config.canonical_root.clone(),
                server_id: id.clone(),
            };
            let (status, reason) =
                if config.preparation_blocked || entry.definition.is_none() && !entry.tombstone {
                    (McpServerStatus::Invalid, Some(McpErrorKind::Configuration))
                } else if entry.tombstone || entry.definition.as_ref().is_some_and(|d| !d.enabled) {
                    (McpServerStatus::Disabled, None)
                } else if entry
                    .definition
                    .as_ref()
                    .is_some_and(|d| !d.roles.iter().any(|role| role == request.role.config_key()))
                {
                    // Role filtering is local to this run, not a change to the
                    // project's server state. In particular, do not publish off.
                    continue;
                } else {
                    let definition = entry.definition.as_ref().expect("checked definition");
                    let result = self
                        .inner
                        .prepare_server(
                            &scope,
                            config.revision,
                            definition,
                            &request.cancel,
                            None,
                            Some(deadline),
                        )
                        .await;
                    match result {
                        Ok(session) => {
                            servers.push(McpServerTools {
                                server_id: id.clone(),
                                transport: match definition.transport {
                                    McpTransportDefinition::Stdio { .. } => McpToolTransport::Stdio,
                                    McpTransportDefinition::Http { .. } => McpToolTransport::Http,
                                },
                                approval_fingerprint: session.fingerprint,
                                definitions: session.definitions.clone(),
                            });
                            transports.insert(
                                id.clone(),
                                match definition.transport {
                                    McpTransportDefinition::Stdio { .. } => McpToolTransport::Stdio,
                                    McpTransportDefinition::Http { .. } => McpToolTransport::Http,
                                },
                            );
                            sessions.insert(id.clone(), session);
                            (McpServerStatus::Ready, None)
                        }
                        Err(kind) => (status_for(kind), Some(kind)),
                    }
                };
            let event = McpStatusEvent {
                scope,
                status,
                reason,
            };
            self.inner.publish(event.clone());
            statuses.push(event);
        }
        let snapshot = FrozenMcpSnapshot::from_servers(servers).map_err(|error| error.kind)?;
        let connector = Arc::new(RunConnector {
            inner: self.inner.clone(),
            root: config.canonical_root,
            role: request.role,
            sessions: AsyncMutex::new(sessions),
            cancel: request.cancel.clone(),
            transports,
        });
        let registry = Arc::new(
            McpToolRegistry::from_snapshot(snapshot.clone(), connector)
                .map_err(|error| error.kind)?,
        );
        Ok(RunMcpLease::prepared(
            snapshot,
            statuses,
            registry,
            request.cancel,
        ))
    }
    fn restore_snapshot(&self, request: RestoreMcpRun) -> Result<RunMcpLease, McpErrorKind> {
        // No canonicalize/load/env/DNS/credentials/admission, or task spawning.
        if !request.context.canonical_root.is_absolute() {
            return Err(McpErrorKind::Configuration);
        }
        let transports = request
            .snapshot
            .tools
            .iter()
            .map(|tool| (tool.remote.server_id.clone(), tool.transport))
            .collect();
        let connector = Arc::new(RunConnector {
            inner: self.inner.clone(),
            root: request.context.canonical_root,
            role: request.context.role,
            sessions: AsyncMutex::new(BTreeMap::new()),
            transports,
            cancel: request.context.cancel.clone(),
        });
        let registry = Arc::new(
            McpToolRegistry::from_snapshot(request.snapshot.clone(), connector)
                .map_err(|e| e.kind)?,
        );
        Ok(RunMcpLease::prepared(
            request.snapshot,
            Vec::new(),
            registry,
            request.context.cancel,
        ))
    }
}
fn process_env(name: &str) -> Option<String> {
    std::env::var(name).ok()
}
fn expand(template: &Template, field: McpValueField) -> Result<String, McpErrorKind> {
    mcp::expand::expand_template(template, &process_env).map_err(|e| match e {
        mcp::McpConfigError::MissingEnvironment => McpErrorKind::MissingEnvironment(field),
        other => mcp_config_error_kind(other),
    })
}
fn secret_error(reason: SecretUnavailableReason) -> McpErrorKind {
    match reason {
        SecretUnavailableReason::Unavailable => McpErrorKind::SecretUnavailable,
        SecretUnavailableReason::Locked => McpErrorKind::SecretLocked,
        SecretUnavailableReason::Denied => McpErrorKind::SecretDenied,
        SecretUnavailableReason::Missing => McpErrorKind::SecretMissing,
    }
}
fn status_for(kind: McpErrorKind) -> McpServerStatus {
    match kind {
        McpErrorKind::NeedsApproval | McpErrorKind::ApprovalRevoked => {
            McpServerStatus::NeedsApproval
        }
        McpErrorKind::Configuration | McpErrorKind::MissingEnvironment(_) => {
            McpServerStatus::Invalid
        }
        McpErrorKind::SecretUnavailable
        | McpErrorKind::SecretLocked
        | McpErrorKind::SecretDenied
        | McpErrorKind::SecretMissing => McpServerStatus::SecretUnavailable,
        McpErrorKind::ProcessLimitReached => McpServerStatus::ProcessLimitReached,
        _ => McpServerStatus::Unavailable,
    }
}
fn executable(command: &str, cwd: &Path) -> Result<PathBuf, McpErrorKind> {
    if command.is_empty() || command.contains('\0') {
        return Err(McpErrorKind::Configuration);
    }
    let candidates = if command.contains(std::path::MAIN_SEPARATOR) {
        vec![cwd.join(command)]
    } else {
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .map(|path| cwd.join(path).join(command))
            .collect()
    };
    candidates
        .into_iter()
        .find_map(|path| {
            let path = std::fs::canonicalize(path).ok()?;
            let metadata = std::fs::metadata(&path).ok()?;
            if !metadata.is_file() {
                return None;
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if metadata.permissions().mode() & 0o111 == 0 {
                    return None;
                }
            }
            Some(path)
        })
        .ok_or(McpErrorKind::Unavailable)
}
