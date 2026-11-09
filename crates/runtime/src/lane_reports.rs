//! One runtime Report boundary: immutable scope/current root, ownership guard,
//! then management CAS. Storage deliberately does not authenticate run generations.
use crate::{AgentRuntime, RunId, lanes::LaneError};
#[cfg(test)]
thread_local! {
    pub(crate) static AFTER_LANE_READ: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
}
impl AgentRuntime {
    pub(crate) fn append_lane_agent_report(
        &self,
        caller: RunId,
        execution_authority: Option<&crate::ownership::OwnerPermit>,
        report: event_bus::LaneReportInput,
    ) -> Result<storage::lanes::LaneReportAppended, LaneError> {
        {
            let runs = self
                .shared
                .runs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let run = runs
                .get(&caller)
                .ok_or_else(|| LaneError::new("lane_unavailable", "caller is not live"))?;
            if run.parent.is_some()
                || run.role != agents::Role::Orchestrator
                || run.config.lane_scope.is_none()
            {
                return Err(LaneError::new(
                    "lane_unavailable",
                    "only a lane root may report",
                ));
            }
        }
        self.validate_run_mutation(caller)
            .map_err(|e| LaneError::new("ownership_lost", e))?;
        // Authenticate the original tool execution, not a replacement entry that
        // happens to reuse the same run ID after restoration.
        let _authority = execution_authority
            .map(|p| p.mutation_guard())
            .transpose()
            .map_err(|e| LaneError::new("ownership_lost", e))?;
        let store = self.shared.run_store.get().ok_or_else(|| {
            LaneError::new("storage_unavailable", "lane ledger is not configured")
        })?;
        let thread = self
            .goal_thread(caller)
            .ok_or_else(|| LaneError::new("lane_unavailable", "no lane thread"))?;
        let lane = store
            .handle
            .lane_by_thread(&thread)?
            .ok_or_else(|| LaneError::new("lane_unavailable", "lane record is missing"))?;
        if lane.root_run_id.as_deref() != Some(&caller.to_string())
            || lane.policy == event_bus::LanePolicy::Detached
        {
            return Err(LaneError::new(
                "lane_unavailable",
                "not the current managed lane root",
            ));
        }
        #[cfg(test)]
        if let Some(hook) = AFTER_LANE_READ.with(|h| h.borrow_mut().take()) {
            hook();
        }
        store
            .handle
            .append_lane_report_with_delivery(
                lane.lane_id,
                lane.management_revision,
                report,
                crate::run_store::review_time_ns(),
            )
            .map_err(Into::into)
    }
}
