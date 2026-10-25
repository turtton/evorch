use super::*;

#[derive(Default)]
pub(super) struct SpawnIntent {
    pub(super) parent: Option<RunId>,
    pub(super) cancelled: bool,
}

impl AgentRuntime {
    /// Linearize image publication against subtree cancellation. The same lock
    /// fences descendants at cancel_subtree; no await or provider work belongs here.
    pub(crate) fn commit_generated_images<T>(
        &self,
        run: RunId,
        publish: impl FnOnce() -> Result<T, String>,
    ) -> Result<T, String> {
        let intents = self
            .shared
            .spawn_intents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if intents.get(&run).is_some_and(|intent| intent.cancelled) {
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
