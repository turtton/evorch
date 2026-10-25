use super::*;

#[derive(Default)]
pub(super) struct SpawnIntent {
    pub(super) parent: Option<RunId>,
    pub(super) cancelled: bool,
    // Separate from inherited subtree cancellation: ordinary cancel fences only
    // this run. Never wait on this gate while holding a global runtime lock.
    pub(super) commit_gate: Arc<Mutex<bool>>,
}

impl AgentRuntime {
    pub(super) fn image_commit_gate(&self, run: RunId) -> Result<Arc<Mutex<bool>>, RuntimeError> {
        let mut intents = self
            .shared
            .spawn_intents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !intents.contains_key(&run)
            && !self
                .shared
                .admissions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .contains_key(&run)
            && !lock_runs(&self.shared.runs).contains_key(&run)
        {
            return Err(unknown_run(run));
        }
        Ok(Arc::clone(&intents.entry(run).or_default().commit_gate))
    }

    /// Linearize publication against both ordinary and subtree cancellation.
    /// Global locks are released before taking the per-run gate or doing I/O.
    pub(crate) fn commit_generated_images<T>(
        &self,
        run: RunId,
        publish: impl FnOnce() -> Result<T, String>,
    ) -> Result<T, String> {
        let gate = self.image_commit_gate(run).map_err(|e| e.to_string())?;
        let cancelled = gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if *cancelled {
            return Err("image generation interrupted; no files saved".into());
        }
        publish()
    }
    /// Records a pre-registration spawn intent so tests can exercise subtree
    /// fencing for runs that are not registered yet.
    #[cfg(test)]
    pub(crate) fn seed_spawn_intent(&self, run: RunId, owner: &str) {
        let Ok(parent) = owner.parse::<RunId>() else {
            return;
        };
        let mut intents = self
            .shared
            .spawn_intents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let cancelled = intents.get(&parent).is_some_and(|intent| intent.cancelled);
        intents.insert(
            run,
            SpawnIntent {
                parent: Some(parent),
                cancelled,
                commit_gate: Arc::new(Mutex::new(cancelled)),
            },
        );
    }

    pub(crate) fn spawn_cancelled(&self, run: RunId) -> bool {
        self.shared
            .spawn_intents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&run)
            .is_some_and(|intent| intent.cancelled)
    }
}
