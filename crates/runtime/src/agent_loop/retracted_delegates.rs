use std::collections::HashMap;

use event_bus::event::{DIAGNOSTIC_SITE_PREFIX, diagnostic_codes};
use event_bus::{DiagnosticEvent, DiagnosticSeverity, Event};
use serde_json::Value;

use super::LoopState;

/// Retractions of one delegate target before the parent's choice is reported.
const REPORT_THRESHOLD: u32 = 3;

/// Detects a parent cancelling background children in the very next model turn
/// after delegating them. A valid but unintended role/category passes target
/// validation, so this repeated self-correction is the only trace it leaves.
#[derive(Default)]
pub(super) struct RetractedDelegates {
    /// Background children spawned since the last model response, with their target.
    spawned: Vec<(String, String)>,
    counts: HashMap<String, u32>,
}

impl RetractedDelegates {
    pub(super) fn record_spawn(&mut self, run_id: String, target: String) {
        self.spawned.push((run_id, target));
    }

    /// Targets whose retraction count reaches the threshold with these calls.
    fn observe(&mut self, calls: &[(String, String, Value)]) -> Vec<(String, u32)> {
        let spawned = std::mem::take(&mut self.spawned);
        let mut reached = Vec::new();
        for (_, name, input) in calls {
            let Some(run_id) = (name == "cancel")
                .then(|| input.get("run_id").and_then(Value::as_str))
                .flatten()
            else {
                continue;
            };
            let Some((_, target)) = spawned.iter().find(|(spawned, _)| spawned == run_id) else {
                continue;
            };
            let count = self.counts.entry(target.clone()).or_default();
            *count += 1;
            if *count == REPORT_THRESHOLD {
                reached.push((target.clone(), *count));
            }
        }
        reached
    }
}

impl LoopState {
    /// Remembers a background child so a cancel in the next turn reads as a retraction.
    pub(crate) fn record_background_delegate(&mut self, run_id: String, target: String) {
        self.retracted_delegates.record_spawn(run_id, target);
    }

    pub(super) fn observe_retracted_delegates(&mut self, calls: &[(String, String, Value)]) {
        for (target, count) in self.retracted_delegates.observe(calls) {
            self.shared.bus.emit(Event::new(DiagnosticEvent {
                source: "retracted_delegates".into(),
                severity: DiagnosticSeverity::Warning,
                code: diagnostic_codes::DELEGATION_RETRACTED.into(),
                detail: format!(
                    "parent cancelled {count} background children in the turn right after delegating them to {target}\n{DIAGNOSTIC_SITE_PREFIX}{target}"
                ),
                run_id: Some(self.task.run_id.to_string()),
                thread_id: None,
                call_id: None,
            }));
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn cancel(run_id: &str) -> (String, String, Value) {
        ("call".into(), "cancel".into(), json!({"run_id": run_id}))
    }

    #[test]
    fn only_next_turn_cancels_of_spawned_children_count_per_target() {
        let mut delegates = RetractedDelegates::default();
        let plan_review = "role=reviewer category=plan-review";
        for turn in 1..=REPORT_THRESHOLD {
            delegates.record_spawn(format!("run-{turn}"), plan_review.into());
            // An unrelated cancel or a cancel of another child is not a retraction.
            let calls = [cancel("run-other"), cancel(&format!("run-{turn}"))];
            let reached = delegates.observe(&calls);
            assert_eq!(
                reached,
                if turn == REPORT_THRESHOLD {
                    vec![(plan_review.to_owned(), REPORT_THRESHOLD)]
                } else {
                    Vec::new()
                }
            );
        }
        // Reported once per target, not on every later retraction.
        delegates.record_spawn("run-late".into(), plan_review.into());
        assert!(delegates.observe(&[cancel("run-late")]).is_empty());

        // Cancelling a child after an intervening turn is ordinary supervision.
        delegates.record_spawn("run-worker".into(), "role=worker category=deep".into());
        assert!(delegates.observe(&[]).is_empty());
        assert!(delegates.observe(&[cancel("run-worker")]).is_empty());
        assert!(!delegates.counts.contains_key("role=worker category=deep"));
    }
}
