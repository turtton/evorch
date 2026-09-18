use std::{path::PathBuf, sync::atomic::Ordering};

use crate::{AgentRuntime, ExecutionPolicy, Role};

impl AgentRuntime {
    /// Applies to newly started runs; running subprocesses retain their namespace.
    pub fn set_sandbox_network(&self, allow_network: bool) {
        self.shared
            .sandbox_allow_network
            .store(allow_network, Ordering::Release);
    }

    pub fn execution_policy(&self, role: Role) -> ExecutionPolicy {
        ExecutionPolicy::for_role(role)
            .with_sandbox_network(self.shared.sandbox_allow_network.load(Ordering::Acquire))
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
