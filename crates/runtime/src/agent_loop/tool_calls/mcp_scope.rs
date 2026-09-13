use super::LoopState;
use crate::network::NetworkAccessDecision;
use crate::scope::{ScopeDecision, ScopeDimension, judge_tool_scope};
use event_bus::{DiagnosticEvent, DiagnosticSeverity, Event, ToolEvent};
use sandbox::PolicyDecision;

impl LoopState {
    pub(super) fn mcp_scope_decision(&self, name: &str) -> NetworkAccessDecision {
        let required = self
            .shared
            .executor
            .tool_permissions(name)
            .map_or_else(Vec::new, |p| {
                [
                    (p.network, ScopeDimension::Network),
                    (p.fs_read, ScopeDimension::FsRead),
                    (p.fs_write, ScopeDimension::FsWrite),
                    (p.process_spawn, ScopeDimension::ProcessSpawn),
                ]
                .into_iter()
                .filter_map(|(required, dimension)| required.then_some(dimension))
                .collect()
            });
        match judge_tool_scope(
            &self.policy.capabilities,
            &self.policy.role_name,
            name,
            &required,
            self.shared
                .executor
                .classify_tool(name)
                .unwrap_or(PolicyDecision::Deny),
            self.task.config.network_access,
        ) {
            ScopeDecision::Allow => NetworkAccessDecision::Allow,
            ScopeDecision::Deny { reason, .. } => NetworkAccessDecision::Deny { reason },
            ScopeDecision::NeedsApproval { reason, .. } => NetworkAccessDecision::Ask { reason },
        }
    }

    pub(super) fn emit_mcp_scope_denial(&self, name: &str, call_id: &str, reason: &str) {
        self.shared.bus.emit(Event::new(ToolEvent::ExecutionDenied {
            tool_name: name.into(),
            call_id: call_id.into(),
            reason: reason.into(),
        }));
        self.shared.bus.emit(Event::new(DiagnosticEvent {
            source: "mcp_scope".into(),
            severity: DiagnosticSeverity::Warning,
            code: "scope_denied".into(),
            detail: reason.into(),
            run_id: Some(self.task.run_id.to_string()),
            thread_id: None,
            call_id: Some(call_id.into()),
        }));
    }
}
