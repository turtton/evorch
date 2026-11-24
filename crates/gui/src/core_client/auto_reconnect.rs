//! What the actor does when the core is lost without a planned restart, and what
//! it tells the person. Pure decisions, tested without a core.
//!
//! An abnormal loss (no `CoreShuttingDown` notice, no planned-restart evidence,
//! the connection really closed) is answered with a *bounded* number of
//! automatic `connect_or_start` attempts: the launcher's shared crash budget
//! decides whether a core may be started, so several clients losing the same
//! core start one. After an orderly shutdown, a refusal or the bound, only a
//! person's explicit reconnect continues.
use core_launcher::{CRASH_BUDGET_LIMIT, CRASH_BUDGET_WINDOW, LaunchError};
use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};
use workbench_protocol::{
    events::{CloseReason, UiEvent},
    namespaces::ProjectionEvent,
};

/// Automatic attempts allowed per window, whatever the core's own budget says.
const MAX_ATTEMPTS: usize = CRASH_BUDGET_LIMIT as usize;
const WINDOW: Duration = CRASH_BUDGET_WINDOW;

#[derive(Default)]
pub struct AutoReconnect {
    attempts: VecDeque<Instant>,
}

impl AutoReconnect {
    /// Take one automatic attempt, or refuse when the window is used up.
    /// Returns the number of this attempt within the window.
    pub fn attempt(&mut self, now: Instant) -> Option<usize> {
        while self
            .attempts
            .front()
            .is_some_and(|at| now.saturating_duration_since(*at) >= WINDOW)
        {
            self.attempts.pop_front();
        }
        if self.attempts.len() >= MAX_ATTEMPTS {
            return None;
        }
        self.attempts.push_back(now);
        Some(self.attempts.len())
    }
}

/// The core announced it is going away on purpose (idle exit, management stop).
pub fn is_orderly_shutdown(event: &UiEvent) -> bool {
    matches!(
        event,
        UiEvent::Projection(ProjectionEvent::SubscriptionClosed {
            reason: CloseReason::CoreShuttingDown
        })
    )
}

pub fn retrying(attempt: usize) -> String {
    format!(
        "Lost the connection to the core unexpectedly. Reconnecting automatically ({attempt}/{MAX_ATTEMPTS})…"
    )
}

pub fn gave_up() -> String {
    format!(
        "The connection to the core was lost more than {MAX_ATTEMPTS} times in {} minutes, so automatic reconnection stopped. Check the cause, then press Reconnect.",
        WINDOW.as_secs() / 60
    )
}

pub fn orderly_stop() -> String {
    "The core stopped (it announced its shutdown). It is not started automatically; press Reconnect to start it again."
        .into()
}

/// The status after a connect-or-start failed. Nothing is retried after this;
/// the text says what to do.
pub fn launch_failure(error: &LaunchError) -> String {
    if error.is_budget_blocked() {
        format!(
            "The core cannot be started automatically (not retried): {error}. Fix the cause, then press Reconnect."
        )
    } else if error.is_incompatible() {
        format!("Cannot connect to the running core (the core was not stopped): {error}")
    } else {
        format!("Connection failed (no fallback): {error}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attempts_are_bounded_per_window_and_come_back_with_it() {
        let mut auto = AutoReconnect::default();
        let t0 = Instant::now();
        assert_eq!(auto.attempt(t0), Some(1));
        assert_eq!(auto.attempt(t0), Some(2));
        assert_eq!(auto.attempt(t0), Some(3));
        assert_eq!(auto.attempt(t0), None);
        assert_eq!(auto.attempt(t0 + WINDOW - Duration::from_millis(1)), None);
        assert_eq!(
            auto.attempt(t0 + WINDOW),
            Some(1),
            "all three left the window"
        );
    }

    #[test]
    fn only_the_shutdown_notice_is_an_orderly_stop() {
        let closed = |reason| UiEvent::Projection(ProjectionEvent::SubscriptionClosed { reason });
        assert!(is_orderly_shutdown(&closed(CloseReason::CoreShuttingDown)));
        assert!(!is_orderly_shutdown(&closed(CloseReason::Unsubscribed)));
        assert!(!is_orderly_shutdown(&closed(CloseReason::ScopeRemoved)));
    }

    #[test]
    fn a_refused_start_names_the_reason_and_promises_no_retry() {
        let refused = LaunchError::Budget(core_launcher::BudgetError::Ledger {
            path: "/c/.evorch-core.crash-budget".into(),
            reason: "not parseable".into(),
        });
        let text = launch_failure(&refused);
        assert!(
            text.contains("not retried") && text.contains("not parseable"),
            "{text}"
        );
        let incompatible = LaunchError::Incompatible {
            detail: "UnsupportedVersion".into(),
            advice: "Update this client.",
        };
        assert!(launch_failure(&incompatible).contains("was not stopped"));
    }
}
