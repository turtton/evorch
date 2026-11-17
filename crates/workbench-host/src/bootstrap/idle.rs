//! Process-retention policy, separate from bootstrap wiring (ADR 0045 stage 18a).
use super::BootstrappedHost;
use event_bus::AgentRunPhase;

impl BootstrappedHost {
    pub fn busy(&self) -> bool {
        // TODO(P9): once safe-point restart/auto-resume can restore an ordinary
        // chat's Waiting state, count that idle conversation as no work. Until
        // then retain all Waiting runs conservatively to avoid losing them.
        self.host.lock().unwrap().service().restart_gate().busy()
            || self.evaluations.is_busy()
            // A live external owner's lease may outlast idle grace. Keep the
            // managed retry alive even before it can publish any local RunEntry.
            || self.recovery_diagnostics.as_ref().is_some_and(|d| d.has_pending_work())
            || self.runtime.has_paused_recovery()
            || self.runtime.pending_lane_stop_writes() != 0
            || !self.runtime.pending_lane_admissions().is_empty()
            || self.host.lock().unwrap().service().has_background_work()
            || self.area_tasks.busy()
            || self.runtime.list_agents().iter().any(|r| {
                self.executor.has_running_shell_jobs(&r.run_id.to_string())
                    || matches!(
                        r.phase,
                        AgentRunPhase::Pending | AgentRunPhase::Running | AgentRunPhase::Waiting
                    )
            })
    }
}

#[cfg(test)]
mod tests;
