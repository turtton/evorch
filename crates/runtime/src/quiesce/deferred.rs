//! Wake decisions are serialized with the restart gate, not regenerated relays.
use super::*;
use crate::{AgentRuntime, RuntimeError};
use dispatch::DispatchGuard;

pub(crate) struct WakePermit<'a> {
    _guard: MutexGuard<'a, Inner>,
}
impl Coordinator {
    pub(crate) fn wake_incarnation(&self, run: RunId) -> Option<String> {
        self.lock().intents.get(&run).map(|i| i.incarnation.clone())
    }
    pub(crate) fn permit_wake(&self, run: RunId, incarnation: &str) -> Option<WakePermit<'_>> {
        let mut inner = self.lock();
        if inner.requested {
            inner.deferred_wakes.insert(run, incarnation.to_string());
            return None;
        }
        Some(WakePermit { _guard: inner })
    }
    pub(crate) fn take_deferred_wakes(&self) -> BTreeMap<RunId, String> {
        let mut inner = self.lock();
        if inner.requested {
            return BTreeMap::new();
        }
        std::mem::take(&mut inner.deferred_wakes)
    }
}
impl AgentRuntime {
    pub(crate) fn dispatch_wake_or_defer(
        &self,
        run: RunId,
        incarnation: &str,
    ) -> Result<Option<DispatchGuard>, RuntimeError> {
        let Some(_permit) = self.shared.quiesce.permit_wake(run, incarnation) else {
            return Ok(None);
        };
        self.dispatch_registration_guard().map(Some)
    }
}
