//! Stop durability is synchronous: a failed write is returned, never queued.
use super::*;

impl AgentRuntime {
    pub(crate) fn persist_lane_start_cancellation(
        &self,
        run: RunId,
        reason: &str,
    ) -> Result<(), LaneError> {
        // This existing creation intent survives a failed Stop/admission teardown,
        // so a repeat Stop can complete the same durable write without a retry queue.
        let lane = self
            .shared
            .lanes
            .creation_intents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&run)
            .cloned();
        let Some(lane) = lane else {
            return Ok(());
        };
        let store = self.shared.run_store.get().ok_or_else(|| {
            LaneError::new("storage_unavailable", "lane ledger is not configured")
        })?;
        store
            .handle
            .block_lane_start(&lane, reason, self.lane_activity_time())?;
        Ok(())
    }
}
