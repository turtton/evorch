//! One canonical model per member, retained from catalog publication to execution.
use super::*;
use std::collections::BTreeMap;

/// Process-local, non-serializable model pins. Obtain these before publishing
/// recovery catalogs; owner retries must resolve fresh pins after any reload.
pub struct RecoveryModels {
    runtime: Weak<Shared>,
    members: BTreeMap<RunId, ModelPin>,
}
struct ModelPin {
    configured: Arc<dyn AgentModel>,
    generation: Arc<dyn AgentModel>,
    concrete: Arc<dyn AgentModel>,
}
impl RecoveryModels {
    /// Publish catalogs on this same model; no provider work is started here.
    pub fn model(&self, run: RunId) -> Option<Arc<dyn AgentModel>> {
        self.members.get(&run).map(|p| Arc::clone(&p.concrete))
    }
    pub(super) fn validate_members(
        &self,
        runtime: &AgentRuntime,
        ids: &BTreeSet<RunId>,
    ) -> Result<(), RuntimeError> {
        if !Weak::ptr_eq(&self.runtime, &Arc::downgrade(&runtime.shared))
            || self.members.keys().copied().collect::<BTreeSet<_>>() != *ids
        {
            return Err(error(
                "recovery model context runtime or membership changed",
            ));
        }
        Ok(())
    }
    pub(super) fn current(
        &self,
        runtime: &AgentRuntime,
        run: RunId,
        config: &RunConfig,
    ) -> Result<Arc<dyn AgentModel>, RuntimeError> {
        let pin = self
            .members
            .get(&run)
            .ok_or_else(|| error("recovery model pin missing"))?;
        let configured = runtime.shared.model_for(config);
        let generation = configured
            .recovery_snapshot()
            .map(|s| s.generation)
            .unwrap_or_else(|| Arc::clone(&configured));
        if !Arc::ptr_eq(&configured, &pin.configured) || !Arc::ptr_eq(&generation, &pin.generation)
        {
            return Err(error(
                "recovery project model reloaded; renew catalogs and authority",
            ));
        }
        Ok(Arc::clone(&pin.concrete))
    }
}
impl AgentRuntime {
    /// Resolve exactly the canonical project context used by execution. A
    /// SwitchableModel's wrapper identity alone is not a configuration fence.
    pub fn pin_recovery_models(&self, authority: &CurrentRecoveryAuthority) -> RecoveryModels {
        RecoveryModels {
            runtime: Arc::downgrade(&self.shared),
            members: authority
                .members
                .iter()
                .map(|(id, config)| {
                    let configured = self.shared.model_for(config);
                    let snapshot = configured.recovery_snapshot().unwrap_or_else(|| {
                        crate::RecoveryModelSnapshot {
                            generation: Arc::clone(&configured),
                            model: Arc::clone(&configured),
                        }
                    });
                    (
                        *id,
                        ModelPin {
                            configured,
                            generation: snapshot.generation,
                            concrete: snapshot.model,
                        },
                    )
                })
                .collect(),
        }
    }
}
