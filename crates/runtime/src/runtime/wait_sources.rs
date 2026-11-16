//! Memory-only wait projections. Never read a parent's output or question state.
use super::{AgentRuntime, RunId, lock_runs};
use crate::mailbox::CompletionNotice;
use event_bus::AgentRunPhase;
use std::sync::Arc;

pub(crate) struct WaitRun {
    pub(crate) id: RunId,
    pub(crate) parent: bool,
    pub(crate) phase: AgentRunPhase,
    pub(crate) notice: Option<Arc<CompletionNotice>>,
}
impl WaitRun {
    pub(crate) fn terminal(&self) -> bool {
        matches!(
            self.phase,
            AgentRunPhase::Done | AgentRunPhase::Error | AgentRunPhase::Stopped
        )
    }
    pub(crate) fn ready(&self) -> bool {
        self.terminal() || self.notice.as_ref().is_some_and(|n| !n.observed())
    }
    pub(crate) fn parent_key(&self) -> String {
        format!(
            "parent:{}:{}",
            self.id,
            self.notice.as_ref().map_or(0, |n| n.generation)
        )
    }
}
impl AgentRuntime {
    pub(crate) fn refresh_wait_lanes(&self, caller: RunId) {
        let lanes = self
            .lane_collection_state(caller)
            .map(|(managed, _)| managed.into_iter().collect())
            .unwrap_or_default();
        self.shared
            .lanes
            .delivery
            .wait_lanes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(caller, lanes);
    }
    pub(crate) fn wait_lanes(&self, caller: RunId) -> Vec<String> {
        self.shared
            .lanes
            .delivery
            .wait_lanes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&caller)
            .cloned()
            .unwrap_or_default()
    }
    pub(crate) fn wait_lane_deliveries(&self, caller: RunId) -> Vec<String> {
        self.shared
            .lanes
            .delivery
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&caller)
            .map(|pending| pending.claim.notification_ids())
            .unwrap_or_default()
    }

    /// The registered relation is authority. Admission is locked before runs,
    /// matching registration; pending reviewers are ordinary direct children.
    pub(crate) fn wait_runs(
        &self,
        caller: RunId,
        explicit: &[RunId],
    ) -> Result<Vec<WaitRun>, String> {
        let admissions = self
            .shared
            .admissions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let runs = lock_runs(&self.shared.runs);
        let own = runs
            .get(&caller)
            .ok_or_else(|| super::unknown_run(caller).to_string())?;
        for target in explicit {
            if *target == caller
                || !(own.parent == Some(*target)
                    || runs
                        .get(target)
                        .is_some_and(|entry| entry.parent == Some(caller))
                    || admissions
                        .get(target)
                        .is_some_and(|a| a.snapshot().parent == Some(caller)))
            {
                return Err(serde_json::json!({"code":"wait_denied","run_id":target.to_string(),"message":"wait requires distinct directly related parent/child runs"}).to_string());
            }
        }
        let mut result = Vec::new();
        for (id, entry) in runs.iter() {
            let parent = own.parent == Some(*id);
            if !parent && entry.parent != Some(caller) {
                continue;
            }
            if !parent && !explicit.is_empty() && !explicit.contains(id) {
                continue;
            }
            let phase = *entry.phase_rx.borrow();
            let terminal = matches!(
                phase,
                AgentRunPhase::Done | AgentRunPhase::Error | AgentRunPhase::Stopped
            );
            let notice = if terminal {
                Some(entry.completion_notice.clone())
            } else if !parent && phase == AgentRunPhase::Waiting {
                entry.idle_turn.as_ref().map(|t| t.notice.clone())
            } else {
                None
            };
            if explicit.is_empty()
                && !parent
                && terminal
                && notice.as_ref().is_some_and(|n| n.observed())
            {
                continue;
            }
            result.push(WaitRun {
                id: *id,
                parent,
                phase,
                notice,
            });
        }
        for (id, admission) in admissions.iter() {
            if runs.contains_key(id)
                || admission.snapshot().parent != Some(caller)
                || (!explicit.is_empty() && !explicit.contains(id))
            {
                continue;
            }
            let phase = if admission.snapshot().result.is_some_and(|r| r.is_err()) {
                AgentRunPhase::Error
            } else {
                AgentRunPhase::Pending
            };
            result.push(WaitRun {
                id: *id,
                parent: false,
                phase,
                notice: None,
            });
        }
        result.sort_by_key(|run| run.id.to_string());
        Ok(result)
    }
}
