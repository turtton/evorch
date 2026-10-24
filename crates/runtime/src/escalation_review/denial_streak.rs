//! Per-run breaker for consecutive automatic escalation-review denials.
//!
//! A run that keeps asking for escalations the reviewer denies is probing for
//! a wording that passes. After [`CONSECUTIVE_DENIAL_LIMIT`] consecutive
//! reviewer denials, further requests in that run are refused without review
//! until the user sends new input to the conversation.

use std::{
    collections::HashMap,
    sync::{Mutex, PoisonError},
};

use crate::RunId;

pub(crate) const CONSECUTIVE_DENIAL_LIMIT: u32 = 3;

/// Appended to every escalation denial the agent sees.
pub(crate) const DENIAL_GUIDANCE: &str = "Do not pursue the same goal through a workaround, a reworded request, or indirect execution. If no safer alternative exists, consult instead: ask_user (a root run asks the user; a subagent asks its parent), escalate when available, or report the blocker in your result.";

struct Streak {
    denials: u32,
    /// Real user inputs visible to the run when the streak started; a change
    /// means the user spoke since, which ends the streak.
    user_inputs: usize,
    last_reason: String,
}

#[derive(Default)]
pub(crate) struct DenialStreaks {
    runs: Mutex<HashMap<RunId, Streak>>,
}

impl DenialStreaks {
    /// The last denial reason while the run is suspended: the limit is reached
    /// and the user has sent no new input since. New input drops the streak.
    pub(crate) fn suspension(&self, run_id: RunId, user_inputs: usize) -> Option<String> {
        let mut runs = self.runs.lock().unwrap_or_else(PoisonError::into_inner);
        let streak = runs.get(&run_id)?;
        if streak.user_inputs != user_inputs {
            runs.remove(&run_id);
            return None;
        }
        (streak.denials >= CONSECUTIVE_DENIAL_LIMIT).then(|| streak.last_reason.clone())
    }

    /// Counts one reviewer denial and returns the streak length. A streak from
    /// before the user's latest input starts over.
    pub(crate) fn record_denial(&self, run_id: RunId, user_inputs: usize, reason: &str) -> u32 {
        let mut runs = self.runs.lock().unwrap_or_else(PoisonError::into_inner);
        let streak = runs.entry(run_id).or_insert(Streak {
            denials: 0,
            user_inputs,
            last_reason: String::new(),
        });
        if streak.user_inputs != user_inputs {
            streak.denials = 0;
            streak.user_inputs = user_inputs;
        }
        streak.denials += 1;
        reason.clone_into(&mut streak.last_reason);
        streak.denials
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

    fn run(ids: &crate::run_ids::RunIds) -> RunId {
        ids.next()
    }

    #[test]
    fn suspends_at_the_limit_until_user_input_changes() {
        let ids = crate::run_ids::RunIds::default();
        let (a, b) = (run(&ids), run(&ids));
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
        let ids = crate::run_ids::RunIds::default();
        let a = run(&ids);
        let streaks = DenialStreaks::default();
        streaks.record_denial(a, 1, "x");
        streaks.record_denial(a, 1, "x");
        assert_eq!(streaks.record_denial(a, 2, "y"), 1);
        streaks.clear(a);
        assert_eq!(streaks.denials(a), 0);
        assert_eq!(streaks.suspension(a, 2), None);
    }
}
