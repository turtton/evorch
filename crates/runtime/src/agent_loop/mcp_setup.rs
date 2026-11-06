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
    pub(super) async fn prepare_mcp_run(&mut self, root: Option<PathBuf>) {
        let Some(runtime) = self.runtime() else {
            return;
        };
        let Some(service) = runtime.shared.mcp_service.get().cloned() else {
            return;
        };
        let Some(root) = root
            .or_else(|| self.task.config.project_root.clone())
            .or_else(|| self.shared.executor.default_cwd())
        else {
            return;
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
        match service
            .prepare_new_run(crate::mcp::PrepareMcpRun {
                canonical_root: root,
                run_id: self.task.run_id.to_string(),
                role: self.task.role,
                cancel,
            })
            .await
        {
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
                    }
                }
            }
            Err(kind) => self.mcp_diagnostic(kind, None),
        }
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
