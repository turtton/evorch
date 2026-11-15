//! Logical adoption fixtures. No production bootstrap calls these APIs.
use super::*;
use crate as runtime;
#[path = "../../shared-test-support/restart_fixture.rs"]
mod fixture;
impl AgentRuntime {
    /// Explicit fixture upgrade; a retry models the same adopted successor epoch.
    pub fn install_planned_recovery_fixture(&self) -> Result<(), RuntimeError> {
        if self.has_planned_restart_handoff() {
            return Ok(());
        }
        let store = self
            .shared
            .run_store
            .get()
            .ok_or_else(|| crate::quiesce::runtime::error("fixture needs storage"))?;
        let proof = store
            .with_database(|db| fixture::inherited_fixture_proof(db, &store.handle))
            .map_err(crate::quiesce::runtime::error)?;
        match proof {
            Some(proof) => self
                .adopt_restart_handoff(proof)
                .map_err(|failure| failure.error),
            None => Ok(()),
        }
    }
}
