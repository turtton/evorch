use std::{path::PathBuf, sync::atomic::Ordering};

use crate::{AgentRuntime, ExecutionPolicy, Role};

impl AgentRuntime {
    pub fn set_sandbox_escalation(
        &self,
        approval: config::EscalationApproval,
        escalate_to_user_on_deny: bool,
    ) {
        *self
            .shared
            .sandbox_escalation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            (approval, escalate_to_user_on_deny);
    }

    pub(crate) fn configure_shell_escalation(&self, executor: &tools::ToolExecutor) {
        executor.set_shell_escalation(
            self.shell_escalation_gate(),
            sandbox::composition::unsandboxed(),
        );
    }

    pub fn shell_escalation_gate(
        &self,
    ) -> std::sync::Arc<crate::escalation_review::SandboxEscalationGate> {
        std::sync::Arc::new(
            crate::escalation_review::SandboxEscalationGate::new(
                self.shared.sandbox_escalation.clone(),
                Some(std::sync::Arc::new(
                    crate::escalation_review::QuickModelReviewer::new(self.shared.model.clone()),
                )),
                self.shared.bus.clone(),
            )
            .with_runtime(std::sync::Arc::downgrade(&self.shared)),
        )
    }

    /// Applies to newly started runs; running subprocesses retain their namespace.
    pub fn set_sandbox_network(&self, allow_network: bool) {
        self.shared
            .sandbox_allow_network
            .store(allow_network, Ordering::Release);
    }

    pub fn execution_policy(&self, role: Role) -> ExecutionPolicy {
        let (approval, fallback) = self
            .shared
            .sandbox_escalation
            .lock()
            .map_or((config::EscalationApproval::Off, false), |settings| {
                *settings
            });
        ExecutionPolicy::for_role(role)
            .with_sandbox_network(self.shared.sandbox_allow_network.load(Ordering::Acquire))
            .with_escalation_approval(approval)
            .with_escalate_to_user_on_deny(fallback)
    }

    /// Enables role-specific production sandboxes for shared-workspace runs.
    pub fn with_sandbox_root(self, root: PathBuf) -> Self {
        *self
            .shared
            .sandbox_root
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(root);
        self
    }
}
