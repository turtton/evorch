//! Runtime wiring for conversation artifacts (ADR 0029).
use std::sync::Arc;

use crate::{AgentRuntime, RunId};

impl AgentRuntime {
    /// Store artifacts captured by `render_artifact` under `store`.
    /// Without a store both artifact tools fail closed.
    pub fn with_artifact_store(self, store: Arc<crate::artifacts::ArtifactStore>) -> Self {
        let _ = self.shared.artifacts.set(store);
        self
    }

    pub(crate) fn artifact_store(&self) -> Option<Arc<crate::artifacts::ArtifactStore>> {
        self.shared.artifacts.get().cloned()
    }

    /// The root of `run`'s delegation tree (ADR 0022).
    pub(crate) fn run_root(&self, run: RunId) -> Result<RunId, String> {
        let mut root = run;
        loop {
            let entry = self.entry(root).map_err(|error| error.to_string())?;
            match entry.parent {
                Some(parent) => root = parent,
                None => return Ok(root),
            }
        }
    }

    pub(crate) fn validate_artifact_mutation(&self, run: RunId) -> Result<(), String> {
        self.validate_run_mutation(run)
            .map_err(|error| error.to_string())
    }
}
