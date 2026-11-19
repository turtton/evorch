//! Logical adoption fixtures. No production bootstrap calls these APIs.
use super::*;
use crate as runtime;
#[allow(dead_code)]
#[path = "../../shared-test-support/restart_fixture.rs"]
mod fixture;
impl AgentRuntime {
    /// Explicit fixture upgrade; a retry models the same adopted successor epoch.
    pub fn install_planned_recovery_fixture(&self) -> Result<(), RuntimeError> {
        self.install_planned_recovery_fixture_with_metadata(
            "fixture-build",
            "fixture-build",
            Default::default(),
        )
    }
    /// Logical fixture only: callers explicitly model a build/catalog transition.
    pub fn install_planned_recovery_fixture_with_metadata(
        &self,
        old_build: &str,
        successor_build: &str,
        old_builtin_tool_names: std::collections::BTreeSet<String>,
    ) -> Result<(), RuntimeError> {
        if self.has_planned_restart_handoff() {
            return Ok(());
        }
        let store = self
            .shared
            .run_store
            .get()
            .ok_or_else(|| crate::quiesce::runtime::error("fixture needs storage"))?;
        let proof = store
            .with_database(|db| {
                fixture::inherited_fixture_proof_with_metadata(
                    db,
                    &store.handle,
                    |id| {
                        let mut metadata = fixture::fixture_restart_metadata(id);
                        metadata.old_build = old_build.into();
                        metadata.successor_build = successor_build.into();
                        metadata.old_builtin_tool_names = old_builtin_tool_names;
                        metadata
                    },
                    |member, metadata| {
                        use storage::StorageError;
                        let fail = |e: String| StorageError::Io(e);
                        let row = db
                            .resume_intent(&member.run_id)?
                            .ok_or_else(|| fail("missing fixture intent".into()))?;
                        let intent = crate::quiesce::ResumeIntent::decode(row).map_err(fail)?;
                        let Some(input) = intent.provider_input else {
                            return Ok(None);
                        };
                        let role =
                            Role::from_name(&intent.role).map_err(|e| fail(e.to_string()))?;
                        let permissions = super::restart_handoff::policy_fingerprint(
                            &self.execution_policy(role),
                        )
                        .map_err(|e| fail(e.to_string()))?;
                        let executor = self
                            .shared
                            .executor
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        let catalog = crate::agent_loop::tool_catalog::builtin_catalog(
                            &executor,
                            &input.request.tools,
                        )
                        .into_iter()
                        .filter(|tool| metadata.old_builtin_tool_names.contains(&tool.name))
                        .collect::<Vec<_>>();
                        super::restart_handoff::tool_snapshot(&input, &catalog, &permissions)
                            .map(Some)
                            .map_err(|e| fail(e.to_string()))
                    },
                )
            })
            .map_err(crate::quiesce::runtime::error)?;
        match proof {
            Some(proof) => {
                self.adopt_restart_handoff(proof)
                    .map_err(|failure| failure.error)?;
                // This seam models post-Ready graph startup, never physical boot.
                let tx = self.adopted_restart_transaction()?;
                if tx.ready_at_ms.is_none() {
                    store
                        .handle
                        .mutate_restart(
                            tx.metadata.restart_id,
                            Some(tx.revision),
                            storage::recovery::restart::RestartMutation::Ready { at_ms: 0 },
                        )
                        .map_err(crate::quiesce::runtime::error)?;
                }
                Ok(())
            }
            None => Ok(()),
        }
    }
}
