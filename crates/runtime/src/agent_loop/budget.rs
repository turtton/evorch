use event_bus::event::diagnostic_codes;

use super::LoopState;
use super::stop_context::stop_reason;
use crate::budget_tracker::{BudgetContext, BudgetDecision};

impl LoopState {
    /// Successful provider presentation, with current ledger authority rechecked.
    /// A receipt or a restored/compacted reference is not collection.
    pub(super) fn observe_lane_presentations(&mut self, visible: &[providers::Message]) -> bool {
        use sha2::{Digest, Sha256};
        let runtime = self.runtime();
        let lead = runtime
            .as_ref()
            .and_then(|runtime| runtime.goal_thread(self.task.run_id));
        let store = runtime
            .as_ref()
            .and_then(|runtime| runtime.shared.run_store.get());
        let texts: std::collections::BTreeMap<_, _> =
            crate::budget_tracker::presentation_texts(visible)
                .map(|text| (format!("{:x}", Sha256::digest(text)), text))
                .collect();
        for pending in &mut self.budget.spawn_credits.lane_presentations {
            pending.lanes.retain(|id| {
                let (Some(store), Some(lead), Some(text)) =
                    (store, lead.as_deref(), texts.get(&pending.fingerprint))
                else {
                    return false;
                };
                presentation_is_current(&store.handle, lead, id, text).unwrap_or(false)
            });
        }
        self.budget.observe_lane_presentations(visible)
    }

    pub(super) fn publish_budget(&mut self) -> BudgetDecision {
        if let Some(runtime) = self.runtime() {
            let started = runtime
                .shared
                .lanes
                .started
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&self.task.run_id)
                .unwrap_or_default();
            for lane in started {
                self.budget.mark_spawned(&lane);
            }
        }
        let run_id = self.task.run_id.to_string();
        let task_id = self
            .task
            .config
            .task_id
            .as_deref()
            .or_else(|| {
                self.task
                    .config
                    .team_task
                    .as_ref()
                    .map(|task| task.id.as_str())
            })
            .unwrap_or(&run_id);
        let decision = self.budget.publish(
            self.escalation_detector.tool_calls(),
            &BudgetContext {
                bus: &self.shared.bus,
                run_id: &run_id,
                task_id,
                settings: &self.task.config.budget,
            },
        );
        match &decision {
            BudgetDecision::Continue => {}
            BudgetDecision::Exhausted(breach) => {
                // A no-progress loop is judged by what it kept calling; other limits are not.
                let last_tool = (breach.code == diagnostic_codes::NO_PROGRESS)
                    .then_some(self.last_tool.as_ref())
                    .flatten();
                let reason = stop_reason(breach.code, last_tool, &breach.detail);
                self.finish_error(reason);
                return decision;
            }
        }
        self.update_durable_artifact();
        decision
    }
}

/// Inspect only full structured reports in the exact visible body. Notification
/// references alone and free text can never acknowledge completion.
fn presentation_is_current(
    store: &storage::StorageHandle,
    lead: &str,
    id: &str,
    text: &str,
) -> Result<bool, storage::StorageError> {
    let Some(lane) = store.lane(id)? else {
        return Ok(false);
    };
    if lane.lead_thread_id.as_deref() != Some(lead) {
        return Ok(false);
    }
    let valid = |report: &event_bus::LaneReport| {
        report.lane_id == id
            && matches!(
                report.status,
                event_bus::LaneStatus::Ready | event_bus::LaneStatus::Failed
            )
            && report.management_revision == lane.management_revision
            && report.brief_revision == lane.brief_revision
    };
    let latest = store.lane_reports(id, lane.latest_report_seq.saturating_sub(1), 1)?;
    if !latest.first().is_some_and(valid) {
        return Ok(false);
    }
    // lane_list returns one JSON page; delivery uses newline-separated envelopes.
    let values = std::iter::once(text)
        .chain(text.lines())
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok());
    for value in values {
        let reports = value["items"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|item| &item["latest_report"])
            .chain(value.get("report"));
        for report in reports {
            if let Ok(report) = serde_json::from_value::<event_bus::LaneReport>(report.clone())
                && valid(&report)
            {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests;
