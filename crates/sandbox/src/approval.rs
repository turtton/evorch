//! Registry-backed approval gate. Bus messages are notifications only.
use event_bus::EventBus;
use std::{sync::Arc, time::Duration};
mod registry;
pub use registry::*;
#[cfg(test)]
mod tests;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalOutcome {
    Approved,
    Denied,
    TimedOut,
    Cancelled,
}

/// Keeps the existing construction API; every gate on a core bus shares authority.
pub struct ApprovalGate {
    event_bus: Arc<EventBus>,
    timeout: Duration,
}
impl ApprovalGate {
    pub const fn new(event_bus: Arc<EventBus>, timeout: Duration) -> Self {
        Self { event_bus, timeout }
    }
    pub fn registry(&self) -> Arc<ApprovalRegistry> {
        ApprovalRegistry::for_bus(&self.event_bus)
    }
    pub async fn request(&self, tool_name: &str, call_id: &str) -> ApprovalOutcome {
        self.request_with_input(tool_name, call_id, None).await
    }
    /// Convenience API for unbound callers. Runtime/tool paths use request_for.
    pub async fn request_with_input(
        &self,
        tool_name: &str,
        call_id: &str,
        input: Option<serde_json::Value>,
    ) -> ApprovalOutcome {
        self.request_for(ApprovalRequest {
            run_id: "unbound",
            thread_id: None,
            tool_name,
            call_id,
            input,
        })
        .await
    }
    pub async fn request_for(&self, request: ApprovalRequest<'_>) -> ApprovalOutcome {
        match self.registry().register(request, self.timeout).wait().await {
            ApprovalState::Approved => ApprovalOutcome::Approved,
            ApprovalState::Denied => ApprovalOutcome::Denied,
            ApprovalState::Expired => ApprovalOutcome::TimedOut,
            ApprovalState::Cancelled | ApprovalState::Pending => ApprovalOutcome::Cancelled,
        }
    }
}
