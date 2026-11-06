//! Privileged GUI operations. Never registered as agent tools.
use super::*;
use crate::mcp::{McpGuiService, McpSettingsView, SaveMcpSecret};
use config::mcp::{McpOverrideMutation, ProjectMcpConfig};
use tools::mcp::{
    secrets::{credential_error_reason, credential_key},
    types::{ConfirmMcpApproval, McpApprovalPreview},
};
fn load_scope(
    scope: &ApprovalScope,
    expected: SourceRevision,
    stale: McpErrorKind,
) -> Result<(ProjectMcpConfig, McpServerDefinition), McpErrorKind> {
    let config = mcp::load_project(&scope.canonical_root).map_err(mcp_config_error_kind)?;
    if config.canonical_root != scope.canonical_root {
        return Err(McpErrorKind::Configuration);
    }
    if config.revision != expected {
        return Err(stale);
    }
    if config.preparation_blocked {
        return Err(McpErrorKind::Configuration);
    }
    let definition = config
        .servers
        .get(&scope.server_id)
        .and_then(|entry| entry.definition.clone())
        .ok_or(McpErrorKind::Configuration)?;
    Ok((config, definition))
}
fn stdio_inspection(
    scope: &ApprovalScope,
    revision: SourceRevision,
    definition: &McpServerDefinition,
) -> Result<McpApprovalInspection, McpErrorKind> {
    let McpTransportDefinition::Stdio { command, cwd, .. } = &definition.transport else {
        return Err(McpErrorKind::Configuration);
    };
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
    inspect_stdio(scope.clone(), revision, definition, &executable, &cwd)
}
fn parsed_url(definition: &McpServerDefinition) -> Result<url::Url, McpErrorKind> {
    let McpTransportDefinition::Http { url, .. } = &definition.transport else {
        return Err(McpErrorKind::Configuration);
    };
    let value = expand(url, McpValueField::Url)?;
    mcp::validate::validate_url(&value).map_err(mcp_config_error_kind)?;
    url::Url::parse(&value).map_err(|_| McpErrorKind::Configuration)
}
impl ServiceInner {
    // Explicit user preview may resolve DNS, never credentials or MCP discovery.
    async fn preview_inspection(
        &self,
        scope: &ApprovalScope,
        revision: SourceRevision,
        definition: &McpServerDefinition,
    ) -> Result<McpApprovalInspection, McpErrorKind> {
        match &definition.transport {
            McpTransportDefinition::Stdio { .. } => stdio_inspection(scope, revision, definition),
            McpTransportDefinition::Http { .. } => {
                let parsed = parsed_url(definition)?;
                let checked = self
                    .guard
                    .inspect_mcp_destination(parsed.as_str())
                    .await
                    .map_err(|_| McpErrorKind::Transport)?;
                inspect_http(
                    scope.clone(),
                    revision,
                    definition,
                    &parsed,
                    if checked.requires_approval {
                        checked.ips
                    } else {
                        Vec::new()
                    },
                )
            }
        }
    }
    fn offline_inspection(
        &self,
        scope: &ApprovalScope,
        revision: SourceRevision,
        definition: &McpServerDefinition,
    ) -> Result<McpApprovalInspection, McpErrorKind> {
        match &definition.transport {
            McpTransportDefinition::Stdio { .. } => stdio_inspection(scope, revision, definition),
            McpTransportDefinition::Http { .. } => {
                let parsed = parsed_url(definition)?;
                let ips = self.approvals.approved_ips(scope)?;
                // Do not imply permission for an unchecked loopback/LAN destination.
                if ips.is_empty()
                    && (parsed.scheme() == "http"
                        || parsed.host_str() == Some("localhost")
                        || parsed.host_str().is_some_and(|host| {
                            host.trim_matches(['[', ']'])
                                .parse::<std::net::IpAddr>()
                                .is_ok_and(|ip| match ip {
                                    std::net::IpAddr::V4(ip) => ip.is_loopback() || ip.is_private(),
                                    std::net::IpAddr::V6(ip) => {
                                        ip.is_loopback() || ip.is_unique_local()
                                    }
                                })
                        }))
                {
                    return Err(McpErrorKind::NeedsApproval);
                }
                inspect_http(scope.clone(), revision, definition, &parsed, ips)
            }
        }
    }
    fn status(&self, scope: ApprovalScope, kind: McpErrorKind) {
        self.publish(McpStatusEvent {
            scope,
            status: status_for(kind),
            reason: Some(kind),
        });
    }
}
#[async_trait]
impl McpGuiService for ProjectMcpService {
    async fn settings(&self, root: PathBuf) -> Result<McpSettingsView, McpErrorKind> {
        let config = mcp::load_project(&root).map_err(mcp_config_error_kind)?;
        let previous = self
            .subscribe_status(config.canonical_root.clone())
            .borrow()
            .clone();
        let mut statuses = Vec::with_capacity(config.servers.len());
        for (id, entry) in &config.servers {
            let scope = ApprovalScope {
                canonical_root: config.canonical_root.clone(),
                server_id: id.clone(),
            };
            let (status, reason) = if config.preparation_blocked || !entry.diagnostics.is_empty() {
                (McpServerStatus::Invalid, Some(McpErrorKind::Configuration))
            } else if entry.tombstone
                || entry
                    .definition
                    .as_ref()
                    .is_some_and(|d| !d.enabled || d.roles.is_empty())
            {
                (McpServerStatus::Disabled, None)
            } else if let Some(definition) = &entry.definition {
                match self
                    .inner
                    .offline_inspection(&scope, config.revision, definition)
                    .and_then(|inspection| self.inner.approvals.check(&inspection))
                {
                    Err(kind) => (status_for(kind), Some(kind)),
                    Ok(_) => previous
                        .iter()
                        .find(|e| e.scope == scope)
                        .map(|e| (e.status, e.reason))
                        .unwrap_or((McpServerStatus::Unavailable, None)),
                }
            } else {
                (McpServerStatus::Invalid, Some(McpErrorKind::Configuration))
            };
            statuses.push(McpStatusEvent {
                scope,
                status,
                reason,
            });
        }
        // Complete bounded replacement prunes notifications for deleted servers.
        if let Ok(mut senders) = self.inner.statuses.lock() {
            senders
                .entry(config.canonical_root.clone())
                .or_insert_with(|| watch::channel(Vec::new()).0)
                .send_replace(statuses.clone());
        }
        Ok(McpSettingsView { config, statuses })
    }
    async fn save_override(
        &self,
        root: PathBuf,
        revision: SourceRevision,
        mutation: McpOverrideMutation,
    ) -> Result<McpSettingsView, McpErrorKind> {
        mcp::save_override(&root, revision, mutation).map_err(mcp_config_error_kind)?;
        self.settings(root).await
    }
    fn subscribe_status(&self, root: PathBuf) -> watch::Receiver<Vec<McpStatusEvent>> {
        ProjectMcpService::subscribe_status(self, root)
    }
    async fn preview_approval(
        &self,
        scope: ApprovalScope,
        expected: SourceRevision,
    ) -> Result<McpApprovalPreview, McpErrorKind> {
        let (_, definition) = load_scope(&scope, expected, McpErrorKind::StalePreview)?;
        let inspection = self
            .inner
            .preview_inspection(&scope, expected, &definition)
            .await?;
        self.inner.approvals.preview(&inspection)
    }
    async fn confirm_approval(&self, confirmation: ConfirmMcpApproval) -> Result<(), McpErrorKind> {
        let (_, definition) = load_scope(
            &confirmation.scope,
            confirmation.expected_revision,
            McpErrorKind::StalePreview,
        )?;
        let inspection = self
            .inner
            .preview_inspection(
                &confirmation.scope,
                confirmation.expected_revision,
                &definition,
            )
            .await?;
        load_scope(
            &confirmation.scope,
            confirmation.expected_revision,
            McpErrorKind::StalePreview,
        )?;
        self.inner.approvals.approve(&confirmation, &inspection)?;
        // No connection and no change to existing run snapshots.
        self.inner.publish(McpStatusEvent {
            scope: confirmation.scope,
            status: McpServerStatus::Unavailable,
            reason: None,
        });
        Ok(())
    }
    async fn revoke_approval(&self, scope: ApprovalScope) -> Result<(), McpErrorKind> {
        let result = self.inner.approvals.revoke(&scope);
        self.inner.status(scope, McpErrorKind::ApprovalRevoked);
        result
    }
    async fn test_connection(
        &self,
        scope: ApprovalScope,
        revision: SourceRevision,
        cancel: CancelContext,
    ) -> Result<McpServerStatus, McpErrorKind> {
        let result = async {
            let (_, definition) = load_scope(&scope, revision, McpErrorKind::ConfigConflict)?;
            if !definition.enabled || definition.roles.is_empty() {
                return Ok(McpServerStatus::Disabled);
            }
            if cancel.is_cancelled() {
                return Err(McpErrorKind::Cancelled);
            }
            let session = self
                .inner
                .prepare_server(&scope, revision, &definition, &cancel)
                .await?;
            // Short-lived lease uses precisely the run admission path and always reaps.
            let valid = match load_scope(&scope, revision, McpErrorKind::ConfigConflict) {
                Ok((_, current)) => self
                    .inner
                    .authorize(&scope, revision, &current)
                    .await
                    .map(|_| ()),
                Err(kind) => Err(kind),
            };
            let cleanup = session
                .raw
                .shutdown(ShutdownReason::Completed)
                .await
                .map_err(|e| e.kind);
            valid?;
            cleanup?;
            if cancel.is_cancelled() {
                return Err(McpErrorKind::Cancelled);
            }
            Ok(McpServerStatus::Ready)
        }
        .await;
        match result {
            Ok(status) => {
                self.inner.publish(McpStatusEvent {
                    scope,
                    status,
                    reason: None,
                });
                Ok(status)
            }
            Err(kind) => {
                self.inner.status(scope, kind);
                Err(kind)
            }
        }
    }
    async fn save_secret(&self, request: SaveMcpSecret) -> Result<McpSettingsView, McpErrorKind> {
        let root = request.scope.canonical_root.clone();
        {
            let _serial = self
                .inner
                .secret_write
                .lock()
                .map_err(|_| McpErrorKind::Unavailable)?;
            let current = mcp::load_project(&root).map_err(mcp_config_error_kind)?;
            if current.canonical_root != root {
                return Err(McpErrorKind::Configuration);
            }
            if current.revision != request.expected_revision {
                return Err(McpErrorKind::ConfigConflict);
            }
            if current.preparation_blocked || request.value.expose().len() > mcp::MAX_STRING_BYTES {
                return Err(McpErrorKind::Configuration);
            }
            mcp::validate::validate_keychain_name(&request.logical_name)
                .map_err(mcp_config_error_kind)?;
            let McpOverrideMutation::Upsert {
                server_id,
                definition,
            } = &request.mutation
            else {
                return Err(McpErrorKind::Configuration);
            };
            if server_id != &request.scope.server_id
                || !references(definition, &request.logical_name)
            {
                return Err(McpErrorKind::Configuration);
            }
            mcp::validate::validate_server(server_id, definition).map_err(mcp_config_error_kind)?;
            let store = match &self.inner.credentials {
                McpCredentialBackend::Available(store) => store,
                McpCredentialBackend::Unavailable(reason) => return Err(secret_error(*reason)),
            };
            let key = credential_key(&request.scope, &request.logical_name);
            let created = store
                .get(&key)
                .map_err(|error| secret_error(credential_error_reason(error)))?
                .is_none();
            store
                .set(&key, &request.value)
                .map_err(|error| secret_error(credential_error_reason(error)))?;
            if let Err(error) =
                mcp::save_override(&root, request.expected_revision, request.mutation)
            {
                // A rotation must never delete a pre-existing key. A concurrent update can
                // also make a newly-created key useful: keep it if referenced or changed.
                let unused = mcp::load_project(&root).is_ok_and(|config| {
                    !config.preparation_blocked
                        && !config
                            .servers
                            .get(&request.scope.server_id)
                            .and_then(|entry| entry.definition.as_ref())
                            .is_some_and(|definition| references(definition, &request.logical_name))
                });
                if created
                    && unused
                    && store
                        .get(&key)
                        .ok()
                        .flatten()
                        .is_some_and(|value| value.expose() == request.value.expose())
                    && let Err(error) = store.delete(&key)
                {
                    self.inner
                        .status(request.scope, secret_error(credential_error_reason(error)));
                }
                return Err(mcp_config_error_kind(error));
            }
        }
        self.settings(root).await
    }
}
fn references(definition: &McpServerDefinition, name: &str) -> bool {
    let matches =
        |source: &ValueSource| matches!(source,ValueSource::Keychain{name:key} if key==name);
    match &definition.transport {
        McpTransportDefinition::Stdio { env, .. } => env.values().any(matches),
        McpTransportDefinition::Http { headers, token, .. } => {
            headers.values().any(matches) || token.as_ref().is_some_and(matches)
        }
    }
}
