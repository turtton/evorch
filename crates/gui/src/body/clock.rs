//! The time the body resolver schedules retries by. Injected, so tests move it
//! by hand instead of waiting.

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// A monotonic clock: the time since some fixed start.
pub trait Clock: Send + Sync {
    fn now(&self) -> Duration;
}

/// The real clock.
pub struct SystemClock {
    start: Instant,
}

impl SystemClock {
    #[must_use]
    pub fn new() -> Self {
        Self {
            start: Instant::now(),
        }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn now(&self) -> Duration {
        self.start.elapsed()
    }
}

/// A clock that only moves when told to.
#[derive(Default)]
pub struct ManualClock {
    now: Mutex<Duration>,
}

impl ManualClock {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn advance(&self, by: Duration) {
        *self.now.lock().unwrap_or_else(|e| e.into_inner()) += by;
    }
}

impl Clock for ManualClock {
    fn now(&self) -> Duration {
        *self.now.lock().unwrap_or_else(|e| e.into_inner())
    }
}
