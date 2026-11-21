//! Memory-only atomic admission; fixed order is public -> collector -> core -> runtime.
use super::{LiveGraphCollector, collector::unavailable};
use crate::admin::{self, AdminFailure};
use std::sync::Arc;
use workbench_protocol::wire::{InstanceEpoch, Revision};
impl LiveGraphCollector {
    /// The transition and canonical return-to-Running predicate must only use
    /// memory. The guard spans BOTH comparison and the sole gate transition.
    pub fn admit<T>(
        &self,
        epoch: InstanceEpoch,
        revision: Revision,
        transition: &mut dyn FnMut() -> Result<T, AdminFailure>,
        returned_to_running: impl FnOnce(&T) -> Arc<dyn Fn() -> bool + Send + Sync>,
    ) -> Result<T, AdminFailure> {
        let mut state = self.state.lock().map_err(|_| unavailable())?;
        state.available()?;
        state.reconcile_restart();
        if state.epoch != Some(epoch)
            || state.revision != revision.0
            || !state.pending.is_empty()
            || state.restart.is_some()
            || state.graphs.values().any(|g| g.undisposed)
        {
            return Err(admin::conflict());
        }
        let id = transition()?;
        state.restart = Some(returned_to_running(&id));
        Ok(id)
    }
}
