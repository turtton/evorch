//! Run-local MCP registration after executor rebuilding and before presentation.
use super::LoopState;
use std::{path::PathBuf, sync::Arc};
use tools::mcp::{McpErrorKind, transport::CancelContext};

pub(super) struct RunMcpCancellation {
    pub cancel: CancelContext,
    bridge: tokio::task::JoinHandle<()>,
}
impl Drop for RunMcpCancellation {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.bridge.abort();
    }
}
impl LoopState {
    pub(crate) fn frozen_mcp_snapshot(&self) -> Option<&tools::mcp::types::FrozenMcpSnapshot> {
        self.mcp_lease
            .as_ref()
            .map(|lease| lease.snapshot())
            .or(self.inherited_mcp_snapshot.as_ref())
    }
    pub(crate) fn mcp_project_root(&self) -> Option<PathBuf> {
        self.mcp_lease
            .as_ref()?
            .statuses()
            .first()
            .map(|status| status.scope.canonical_root.clone())
    }
    pub(super) async fn prepare_mcp_run(&mut self, root: Option<PathBuf>, restore: bool) -> bool {
        let Some(runtime) = self.runtime() else {
            return true;
        };
        let service = match runtime.shared.mcp_service.get().cloned() {
            Some(service) => service,
            None if !restore => return true,
            None => Arc::new(crate::mcp::UnavailableMcpService::new(
                crate::mcp::McpCredentialBackend::Unavailable(
                    crate::mcp::SecretUnavailableReason::Unavailable,
                ),
            )) as Arc<dyn crate::mcp::McpRunService>,
        };
        let Some(root) = root
            .or_else(|| self.task.config.project_root.clone())
            .or_else(|| self.shared.executor.default_cwd())
        else {
            if restore
                && self
                    .inherited_mcp_snapshot
                    .as_ref()
                    .is_some_and(|snapshot| !snapshot.tools.is_empty())
            {
                self.finish_error("Saved MCP project identity is unavailable".into());
                return false;
            }
            return true;
        };
        let cancel = CancelContext::default();
        let token = cancel.clone();
        let mut interrupts = self.channels.cancel_rx.clone();
        let bridge = tokio::spawn(async move {
            loop {
                if interrupts.borrow().kind().is_some() {
                    token.cancel();
                    break;
                }
                if interrupts.changed().await.is_err() {
                    token.cancel();
                    break;
                }
            }
        });
        self.mcp_cancellation = Some(RunMcpCancellation {
            cancel: cancel.clone(),
            bridge,
        });
        let context = crate::mcp::PrepareMcpRun {
            canonical_root: root,
            run_id: self.task.run_id.to_string(),
            role: self.task.role,
            cancel,
        };
        let prepared = if restore {
            service.restore_snapshot(crate::mcp::RestoreMcpRun {
                context,
                snapshot: self.inherited_mcp_snapshot.clone().unwrap_or_default(),
            })
        } else {
            service.prepare_new_run(context).await
        };
        match prepared {
            Ok(lease) => {
                for status in lease.statuses() {
                    if let Some(reason) = status.reason {
                        self.mcp_diagnostic(reason, Some(status));
                    }
                }
                match lease.register_executor(&self.shared.executor, self.shared.bus.clone()) {
                    Ok(executor) => {
                        // These concrete names belong only to this run and role.
                        // Never change Role defaults or the run_js fixed allowlist.
                        self.policy.capabilities.allowed_tools.extend(
                            lease
                                .snapshot()
                                .tools
                                .iter()
                                .map(|tool| tool.public_name.clone()),
                        );
                        self.shared.executor = Arc::new(executor);
                        self.mcp_lease = Some(lease);
                    }
                    Err(kind) => {
                        lease
                            .shutdown(tools::mcp::transport::ShutdownReason::InitializationFailed)
                            .await;
                        self.mcp_diagnostic(kind, None);
                        if restore {
                            self.finish_error("Saved MCP tools cannot be registered".into());
                            return false;
                        }
                    }
                }
            }
            Err(kind) => {
                self.mcp_diagnostic(kind, None);
                if restore {
                    self.finish_error("Saved MCP tools cannot be restored".into());
                    return false;
                }
            }
        }
        true
    }
    fn mcp_diagnostic(
        &self,
        kind: McpErrorKind,
        status: Option<&tools::mcp::types::McpStatusEvent>,
    ) {
        self.shared
            .bus
            .emit(event_bus::Event::new(event_bus::DiagnosticEvent {
                source: "mcp".into(),
                severity: event_bus::DiagnosticSeverity::Warning,
                code: format!("mcp.{kind}"),
                detail: status
                    .and_then(|status| serde_json::to_string(status).ok())
                    .unwrap_or_else(|| kind.to_string()),
                run_id: Some(self.task.run_id.to_string()),
                thread_id: self.thread_id.clone(),
                call_id: None,
            }));
    }
}
