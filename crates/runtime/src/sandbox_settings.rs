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

    /// Add `env` to every shell command of this runtime's runs, including the
    /// executors runs build for their own sandbox. Sandbox-forced variables
    /// (`HOME`, `TMPDIR`, `CARGO_HOME`, `RUSTUP_HOME` under bwrap) still win.
    /// The first value set wins.
    #[must_use]
    pub fn with_shell_environment(self, env: Vec<(String, String)>) -> Self {
        if self.shared.shell_environment.set(env).is_ok() {
            self.configure_shell_environment(
                &self
                    .shared
                    .executor
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
            );
        }
        self
    }

    pub(crate) fn configure_shell_environment(&self, executor: &tools::ToolExecutor) {
        if let Some(env) = self.shared.shell_environment.get() {
            executor.set_shell_environment(env.clone());
        }
    }

    /// Escalated shells keep the run's `TMPDIR` when it has a thread scratch.
    pub(crate) fn configure_shell_escalation(
        &self,
        executor: &tools::ToolExecutor,
        scratch: Option<&std::path::Path>,
    ) {
        executor.set_shell_escalation(
            self.shell_escalation_gate(),
            scratch.map_or_else(sandbox::composition::unsandboxed, |dir| {
                sandbox::composition::unsandboxed_with_tmpdir(dir.to_path_buf())
            }),
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

    /// New runs see the configured Web tools; calls from existing runs are gated too.
    pub fn set_web_tools_enabled(&self, enabled: bool) {
        self.shared
            .web_tools_enabled
            .store(enabled, Ordering::Release);
    }

    pub fn web_tools_enabled(&self) -> bool {
        self.shared.web_tools_enabled.load(Ordering::Acquire)
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
