//! The immutable core-startup fence is shared by manual, background and direct resume.
use super::*;
use std::collections::BTreeSet;

impl AgentRuntime {
    /// Freeze the graph-owned identities before any lane recovery is started.
    /// Legacy/embedded hosts do not call this and keep their empty exclusion set.
    /// A failed scan must prevent the caller from starting lane recovery.
    pub fn fence_graph_lane_recovery(&self) -> Result<(), LaneError> {
        let opening = self
            .shared
            .lanes
            .opening
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !opening.is_empty() {
            return Err(LaneError::new(
                "recovery_conflict",
                "lane recovery already attempted",
            ));
        }
        let store = self.shared.run_store.get().ok_or_else(|| {
            LaneError::new("storage_unavailable", "lane ledger is not configured")
        })?;
        let plan = self
            .recovery_plan()
            .map_err(|_| LaneError::new("graph_recovery_required", "graph scan failed"))?;
        let quarantined = plan
            .report
            .blocked
            .iter()
            .chain(&plan.report.skipped)
            .filter(|outcome| outcome.reason != crate::quiesce::RecoveryReason::SupersededManifest)
            .filter_map(|outcome| outcome.manifest_id.clone())
            .collect();
        let ids = store.with_database(|db| db.recovery_owned_run_ids(&quarantined))?;
        if ids.iter().any(|id| id.parse::<RunId>().is_err()) {
            return Err(LaneError::new(
                "graph_recovery_required",
                "unreadable graph member identity",
            ));
        }
        self.shared.lanes.graph_exclusions.set(ids).map_err(|_| {
            LaneError::new(
                "recovery_conflict",
                "graph exclusion fence already installed",
            )
        })?;
        Ok(())
    }

    pub(super) fn lane_recovery_exclusions(&self) -> &BTreeSet<String> {
        static EMPTY: std::sync::OnceLock<BTreeSet<String>> = std::sync::OnceLock::new();
        self.shared
            .lanes
            .graph_exclusions
            .get()
            .unwrap_or_else(|| EMPTY.get_or_init(BTreeSet::new))
    }

    pub(super) fn check_lane_recovery(
        &self,
        lane: &LaneRecord,
        seed: &StartSeed,
    ) -> Result<(), LaneError> {
        let excluded = self.lane_recovery_exclusions();
        if excluded.contains(&seed.run_id.to_string())
            || lane
                .root_run_id
                .as_ref()
                .is_some_and(|id| excluded.contains(id))
        {
            return Err(LaneError::new(
                "graph_recovery_required",
                "graph-owned lane requires graph recovery or explicit Continue",
            ));
        }
        Ok(())
    }
}
