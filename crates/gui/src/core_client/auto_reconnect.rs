//! What the actor does when the core is lost without a planned restart, and what
//! it tells the person. Pure decisions, tested without a core.
//!
//! An abnormal loss (no `CoreShuttingDown` notice, no planned-restart evidence,
//! the connection really closed) is answered with a *bounded* number of
//! automatic `connect_or_start` attempts: the launcher's shared crash budget
//! decides whether a core may be started, so several clients losing the same
//! core start one. After an orderly shutdown, a refusal or the bound, recovery
//! continues connect-only. Only a session opening or an explicit reconnect can
//! allow starts again; elapsed time alone cannot.
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
pub const MAX_ATTEMPTS: usize = CRASH_BUDGET_LIMIT as usize;
const WINDOW: Duration = CRASH_BUDGET_WINDOW;

/// How long a session must have lasted for the next loss to count as a new one,
/// not as the same trouble coming back.
const STABLE_SESSION: Duration = Duration::from_secs(60);
const LIVE_FIRST_BACKOFF: Duration = Duration::from_secs(1);
const LIVE_MAX_BACKOFF: Duration = Duration::from_secs(30);

/// Pacing of automatic recovery, including failed connects and session reads.
/// This is separate from the 3-in-5-minutes allowance of [`AutoReconnect`],
/// which guards starts. The first retry is immediate; further failures or short
/// sessions wait twice as long, up to 30 seconds. A stable session resets it.
#[derive(Default)]
pub struct LiveCoreBackoff {
    short_sessions: u32,
}

impl LiveCoreBackoff {
    /// Reconnects to a live core in the current run of trouble (reset by a
    /// session that held).
    pub fn attempts(&self) -> u32 {
        self.short_sessions
    }

    /// The wait before the next reconnect; `session` is how long the lost connection had lasted.
    pub fn next_delay(&mut self, session: Duration) -> Duration {
        if session >= STABLE_SESSION {
            self.short_sessions = 0;
        }
        let delay = match self.short_sessions {
            0 => Duration::ZERO,
            n => LIVE_FIRST_BACKOFF
                .saturating_mul(1u32 << (n - 1).min(16))
                .min(LIVE_MAX_BACKOFF),
        };
        self.short_sessions = self.short_sessions.saturating_add(1);
        delay
    }
}

pub struct AutoReconnect {
    attempts: VecDeque<Instant>,
    start_allowed: bool,
}

impl Default for AutoReconnect {
    fn default() -> Self {
        Self {
            attempts: VecDeque::new(),
            start_allowed: true,
        }
    }
}

impl AutoReconnect {
    /// A failed attempt or orderly stop must never turn into periodic starts.
    pub fn connect_only(&mut self) {
        self.start_allowed = false;
    }

    /// A real session or a person's reconnect starts a new recovery opportunity,
    /// but neither clears the rolling allowance nor the launcher's crash budget.
    pub fn allow_start(&mut self) {
        self.start_allowed = true;
    }

    /// Automatic attempts of the allowance that are still inside the window.
    pub fn used(&self, now: Instant) -> usize {
        self.attempts
            .iter()
            .filter(|at| now.saturating_duration_since(**at) < WINDOW)
            .count()
    }

    /// Take one automatic attempt, or refuse when the window is used up.
    /// Returns the number of this attempt within the window.
    pub fn attempt(&mut self, now: Instant) -> Option<usize> {
        if !self.start_allowed {
            return None;
        }
        while self
            .attempts
            .front()
            .is_some_and(|at| now.saturating_duration_since(*at) >= WINDOW)
        {
            self.attempts.pop_front();
        }
        if self.attempts.len() >= MAX_ATTEMPTS {
            self.connect_only();
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

pub fn reconnecting_live(delay: Duration) -> String {
    if delay.is_zero() {
        "Lost the connection to the core, which is still running. Reconnecting…".into()
    } else {
        format!(
            "Lost the connection to the core, which is still running. Reconnecting in {} s…",
            delay.as_secs().max(1)
        )
    }
}

pub fn gave_up() -> String {
    "Automatic core starts are paused. Waiting for a running core…".into()
}

pub fn orderly_stop() -> String {
    "The core announced its shutdown. Waiting for a running core without starting it…".into()
}

/// A short status only; the error itself is kept separately for hover.
pub fn launch_failure(error: &LaunchError) -> String {
    if error.is_budget_blocked() {
        "Core start budget exhausted. Waiting for a running core…".into()
    } else if error.is_incompatible() {
        "Core compatibility mismatch. Waiting for a compatible core…".into()
    } else {
        "Connection failed. Waiting for a running core…".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exhausting_the_allowance_stays_connect_only_even_after_the_window_expires() {
        let mut auto = AutoReconnect::default();
        let t0 = Instant::now();
        assert_eq!(auto.attempt(t0), Some(1));
        assert_eq!(auto.attempt(t0), Some(2));
        assert_eq!(auto.attempt(t0), Some(3));
        assert_eq!(auto.attempt(t0), None);
        assert_eq!(auto.attempt(t0 + WINDOW - Duration::from_millis(1)), None);
        assert_eq!(auto.attempt(t0 + WINDOW), None);
        assert_eq!(auto.attempt(t0 + WINDOW * 2), None);
        auto.allow_start(); // Only a session opening or explicit Reconnect can do this.
        assert_eq!(auto.attempt(t0 + WINDOW * 2), Some(1));
    }

    #[test]
    fn a_failed_start_stays_connect_only_and_manual_reconnect_does_not_clear_the_allowance() {
        let mut auto = AutoReconnect::default();
        let t0 = Instant::now();
        assert_eq!(auto.attempt(t0), Some(1));
        auto.connect_only();
        auto.allow_start();
        assert_eq!(auto.attempt(t0 + Duration::from_secs(1)), Some(2));
        auto.connect_only();
        assert_eq!(auto.attempt(t0 + WINDOW), None);
        assert_eq!(auto.attempt(t0 + WINDOW * 2), None);
    }

    #[test]
    fn a_live_core_is_reconnected_at_a_pace_that_grows_with_short_sessions_and_is_capped() {
        let mut live = LiveCoreBackoff::default();
        let short = Duration::from_secs(2);
        let waits: Vec<_> = (0..8).map(|_| live.next_delay(short).as_secs()).collect();
        assert_eq!(waits, [0, 1, 2, 4, 8, 16, 30, 30]);
        assert_eq!(
            live.attempts(),
            8,
            "every reconnect to a live core is counted"
        );
        assert_eq!(
            live.next_delay(STABLE_SESSION),
            Duration::ZERO,
            "a session that held starts over"
        );
        assert_eq!(live.attempts(), 1);
        assert_eq!(live.next_delay(short), LIVE_FIRST_BACKOFF);
        assert_eq!(live.attempts(), 2);
    }

    #[test]
    fn the_live_core_pace_never_overflows_however_long_the_trouble_lasts() {
        let mut live = LiveCoreBackoff::default();
        for _ in 0..1000 {
            assert!(live.next_delay(Duration::ZERO) <= LIVE_MAX_BACKOFF);
        }
    }

    #[test]
    fn the_attempts_in_use_leave_with_their_window() {
        let mut auto = AutoReconnect::default();
        let t0 = Instant::now();
        auto.attempt(t0);
        auto.attempt(t0 + Duration::from_secs(60));
        assert_eq!(auto.used(t0 + Duration::from_secs(61)), 2);
        assert_eq!(auto.used(t0 + WINDOW), 1, "the first one has expired");
        assert_eq!(auto.used(t0 + WINDOW + Duration::from_secs(60)), 0);
    }

    #[test]
    fn only_the_shutdown_notice_is_an_orderly_stop() {
        let closed = |reason| UiEvent::Projection(ProjectionEvent::SubscriptionClosed { reason });
        assert!(is_orderly_shutdown(&closed(CloseReason::CoreShuttingDown)));
        assert!(!is_orderly_shutdown(&closed(CloseReason::Unsubscribed)));
        assert!(!is_orderly_shutdown(&closed(CloseReason::ScopeRemoved)));
    }
}
