//! Process-retention policy, separate from bootstrap wiring (ADR 0045 stage 18a).
use super::BootstrappedHost;

impl BootstrappedHost {
    pub fn busy(&self) -> bool {
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
            || self.runtime.has_active_runs()
            // P9 recovery does not yet restore questions or approval authority.
            // Preserve even questions belonging to a run not currently loaded.
            || self.runtime.with_storage_reader(|db| {
                db.pending_user_questions().map(|questions| !questions.is_empty())
            }).map_or(true, |pending| pending.unwrap_or(false))
            || sandbox::approval::ApprovalRegistry::existing(self.runtime.event_bus().as_ref())
                .is_some_and(|registry| !registry.list_pending(None, 1).is_empty())
            || self.runtime.list_agents().iter().any(|r| {
                self.executor.has_running_shell_jobs(&r.run_id.to_string())
            })
    }
}

#[cfg(test)]
mod tests;
