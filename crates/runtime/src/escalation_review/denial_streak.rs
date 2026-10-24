//! Per-run breaker for consecutive automatic escalation-review denials.
//!
//! Policy denials count, but environment failures and human refusals do not.
//! New real-user input or a valid approval ends the streak.

use std::{
    collections::HashMap,
    sync::{Mutex, PoisonError},
};

use crate::RunId;

pub(crate) const CONSECUTIVE_DENIAL_LIMIT: u32 = 3;

pub(crate) use tools::tools::shell_escalation::DENIAL_GUIDANCE;

struct Streak {
    denials: u32,
    /// Latest observed input generation, retained even at zero denials so old
    /// completions cannot revive an earlier generation.
    user_inputs: usize,
    last_reason: String,
}

impl Streak {
    fn new(user_inputs: usize) -> Self {
        Self {
            denials: 0,
            user_inputs,
            last_reason: String::new(),
        }
    }
}

#[derive(Default)]
pub(crate) struct DenialStreaks {
    runs: Mutex<HashMap<RunId, Streak>>,
}

impl DenialStreaks {
    /// The last denial reason while suspended. New input resets the count but
    /// retains the generation watermark; stale lookups cannot lift suspension.
    /// This lookup is the handoff-admission linearization point: it shares
    /// `runs`' mutex with `record_denial`, but releases it before any dialog.
    pub(crate) fn suspension(&self, run_id: RunId, user_inputs: usize) -> Option<String> {
        let mut runs = self.runs.lock().unwrap_or_else(PoisonError::into_inner);
        let streak = runs
            .entry(run_id)
            .or_insert_with(|| Streak::new(user_inputs));
        if user_inputs > streak.user_inputs {
            *streak = Streak::new(user_inputs);
        }
        (streak.denials >= CONSECUTIVE_DENIAL_LIMIT).then(|| streak.last_reason.clone())
    }

    /// Counts a policy denial, including authorization failures and missing
    /// real-user requests, not an environment failure or a human refusal.
    pub(crate) fn record_denial(&self, run_id: RunId, user_inputs: usize, reason: &str) -> u32 {
        let mut runs = self.runs.lock().unwrap_or_else(PoisonError::into_inner);
        let streak = runs
            .entry(run_id)
            .or_insert_with(|| Streak::new(user_inputs));
        if user_inputs < streak.user_inputs {
            return streak.denials;
        }
        if user_inputs > streak.user_inputs {
            *streak = Streak::new(user_inputs);
        }
        streak.denials = streak.denials.saturating_add(1);
        reason.clone_into(&mut streak.last_reason);
        streak.denials
    }

    /// Delayed approvals, like delayed denials, belong to their starting input
    /// generation and cannot reset a newer generation's count or suspension.
    /// Keep the watermark after approval; only run termination removes it.
    pub(crate) fn record_approval(&self, run_id: RunId, user_inputs: usize) {
        let mut runs = self.runs.lock().unwrap_or_else(PoisonError::into_inner);
        let streak = runs
            .entry(run_id)
            .or_insert_with(|| Streak::new(user_inputs));
        if user_inputs >= streak.user_inputs {
            *streak = Streak::new(user_inputs);
        }
    }

    pub(crate) fn clear(&self, run_id: RunId) {
        self.runs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&run_id);
    }

    #[cfg(test)]
    pub(crate) fn denials(&self, run_id: RunId) -> u32 {
        self.runs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&run_id)
            .map_or(0, |streak| streak.denials)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suspends_at_the_limit_until_user_input_changes() {
        let ids = crate::run_ids::RunIds::default();
        let (a, b) = (ids.next(), ids.next());
        let streaks = DenialStreaks::default();
        for expected in 1..CONSECUTIVE_DENIAL_LIMIT {
            assert_eq!(streaks.record_denial(a, 1, "unsafe"), expected);
            assert_eq!(streaks.suspension(a, 1), None);
        }
        assert_eq!(
            streaks.record_denial(a, 1, "last reason"),
            CONSECUTIVE_DENIAL_LIMIT
        );
        assert_eq!(streaks.suspension(a, 1).as_deref(), Some("last reason"));
        assert_eq!(streaks.suspension(b, 1), None, "streaks are per run");
        assert_eq!(streaks.suspension(a, 2), None, "new input lifts it");
        assert_eq!(streaks.denials(a), 0);
    }

    #[test]
    fn new_input_restarts_the_count_and_clear_resets() {
        let a = crate::run_ids::RunIds::default().next();
        let streaks = DenialStreaks::default();
        streaks.record_denial(a, 1, "x");
        streaks.record_denial(a, 1, "x");
        assert_eq!(streaks.record_denial(a, 2, "y"), 1);
        streaks.clear(a);
        assert_eq!(streaks.denials(a), 0);
        assert_eq!(streaks.suspension(a, 2), None);
    }

    #[test]
    fn old_completions_cannot_revive_a_cleared_generation() {
        let a = crate::run_ids::RunIds::default().next();
        let streaks = DenialStreaks::default();
        streaks.record_denial(a, 1, "old");
        assert_eq!(streaks.suspension(a, 2), None);
        assert_eq!(streaks.record_denial(a, 1, "late"), 0);
        streaks.record_denial(a, 2, "current");
        streaks.record_approval(a, 2);
        assert_eq!(streaks.record_denial(a, 1, "late"), 0);
        for _ in 0..CONSECUTIVE_DENIAL_LIMIT {
            streaks.record_denial(a, 2, "current");
        }
        streaks.record_approval(a, 1);
        assert_eq!(streaks.suspension(a, 1).as_deref(), Some("current"));
    }
}
