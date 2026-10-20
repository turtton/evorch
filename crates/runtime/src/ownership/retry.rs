//! Bounded retries for ownership registry operations that can safely run again.
//!
//! Only whole-transaction operations whose writes are a pure function of the
//! state read inside the same transaction use this: every attempt opens a new
//! connection, re-reads and re-validates the owner, and drops the connection
//! (rolling back) before waiting. Callbacks with external effects
//! (`Registry::update`, `prepare_child`) are never retried here.
//!
//! Waiting is bounded by absolute deadlines, not per-statement timeouts. An
//! attempt's connection waits for SQLite locks through [`wait_within_budget`],
//! which every statement of that attempt shares: a BEGIN that waited 1.5 s of
//! a 2 s attempt leaves its COMMIT 0.5 s. Each attempt ends no later than the
//! operation deadline, and no attempt starts once that deadline is too close.
use std::cell::RefCell;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::RegistryError;
use super::observe::{DbOperation, TARGET, millis};
use super::registry::ATTEMPT_TIMEOUT;

/// Retry budget for contention on the ownership registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Total lock-wait budget from the first attempt. No attempt waits past
    /// it and none starts once less than a minimal attempt remains.
    pub deadline: Duration,
    /// Combined SQLite lock wait of one attempt, across all of its
    /// statements. Bounds how long an interrupt waits for the attempt in
    /// progress.
    pub attempt_timeout: Duration,
    pub initial_backoff: Duration,
    pub max_backoff: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            deadline: Duration::from_secs(10),
            attempt_timeout: ATTEMPT_TIMEOUT,
            initial_backoff: Duration::from_millis(25),
            max_backoff: Duration::from_millis(500),
        }
    }
}

/// Time source of retry budgets. Tests substitute virtual time.
pub(super) trait Clock: Send + Sync {
    fn now(&self) -> Instant;
    fn sleep(&self, duration: Duration);
}

struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }

    fn sleep(&self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

pub(super) fn system_clock() -> Arc<dyn Clock> {
    Arc::new(SystemClock)
}

/// The absolute end of the attempt running on the current thread.
#[derive(Clone)]
struct AttemptBudget {
    until: Instant,
    clock: Arc<dyn Clock>,
}

thread_local! {
    /// Set only while [`retry_contention`] runs an attempt on this thread.
    static ATTEMPT_BUDGET: RefCell<Option<AttemptBudget>> = const { RefCell::new(None) };
}

/// Clears the attempt budget even if the attempt panics.
struct BudgetScope;

impl BudgetScope {
    fn enter(budget: AttemptBudget) -> Self {
        ATTEMPT_BUDGET.with(|slot| *slot.borrow_mut() = Some(budget));
        Self
    }
}

impl Drop for BudgetScope {
    fn drop(&mut self) {
        ATTEMPT_BUDGET.with(|slot| *slot.borrow_mut() = None);
    }
}

/// SQLite busy handler of attempt connections. It waits only within the
/// current attempt's budget; outside an attempt (or on another thread) it
/// gives up at once, so a connection never waits without a bound.
pub(super) fn wait_within_budget(count: i32) -> bool {
    let Some(budget) = ATTEMPT_BUDGET.with(|slot| slot.borrow().clone()) else {
        return false;
    };
    match busy_delay(count, budget.clock.now(), budget.until) {
        Some(delay) => {
            budget.clock.sleep(delay);
            true
        }
        None => false,
    }
}

/// SQLite's own busy-timeout schedule, truncated at `until`.
fn busy_delay(count: i32, now: Instant, until: Instant) -> Option<Duration> {
    const DELAYS_MS: [u64; 12] = [1, 2, 5, 10, 15, 20, 25, 25, 25, 50, 50, 100];
    let left = until.saturating_duration_since(now);
    if left.is_zero() {
        return None;
    }
    let index = usize::try_from(count).unwrap_or(0);
    let delay = DELAYS_MS.get(index).copied().unwrap_or(100);
    Some(Duration::from_millis(delay).min(left))
}

/// The operation deadline leaves less than this for another attempt.
const MIN_ATTEMPT: Duration = Duration::from_millis(5);
/// Interrupt checks while backing off.
const SLEEP_SLICE: Duration = Duration::from_millis(20);

/// Run `attempt(attempt_number)` until it succeeds, fails with a
/// non-retryable error, the deadline passes or `interrupted` returns true.
/// The attempt must open its connection with
/// [`super::registry::Wait::Budget`] so its lock waits share the budget.
///
/// `interrupted` is checked before every attempt and after every busy one.
/// An interrupt wins over an exhausted deadline: neither commits anything,
/// and the caller then ends as the interrupt. A committed attempt is
/// returned even if an interrupt arrived meanwhile.
///
/// Blocks the calling thread: async callers must use `spawn_blocking`.
pub(super) fn retry_contention<T>(
    db: &Path,
    operation: DbOperation,
    thread_id: &str,
    policy: &RetryPolicy,
    clock: &Arc<dyn Clock>,
    interrupted: &dyn Fn() -> bool,
    mut attempt: impl FnMut(u32) -> Result<T, RegistryError>,
) -> Result<T, RegistryError> {
    let started = clock.now();
    let deadline = started + policy.deadline;
    let mut backoff = policy.initial_backoff;
    let mut attempts = 0;
    let mut last_error = None;
    let interrupt = |attempts| {
        tracing::info!(
            target: TARGET,
            db = %db.display(),
            operation = operation.as_str(),
            thread_id,
            attempts,
            elapsed_ms = millis(clock.now().saturating_duration_since(started)),
            "ownership registry retry interrupted"
        );
        RegistryError::Interrupted {
            operation,
            attempts,
        }
    };
    let expire = |error: RegistryError, attempts| {
        tracing::warn!(
            target: TARGET,
            db = %db.display(),
            operation = operation.as_str(),
            thread_id,
            attempts,
            elapsed_ms = millis(clock.now().saturating_duration_since(started)),
            deadline_ms = millis(policy.deadline),
            sqlite_code = ?error.sqlite_code(),
            "ownership registry contention outlasted the retry deadline"
        );
        error.with_attempts(attempts)
    };
    loop {
        if interrupted() {
            return Err(interrupt(attempts));
        }
        let now = clock.now();
        // Backoff (or rescheduling afterwards) may overshoot the deadline.
        // Keep the last DB error and its count rather than starting a new
        // connection/transaction with no useful budget left.
        if deadline.saturating_duration_since(now) < MIN_ATTEMPT
            && let Some(error) = last_error.take()
        {
            return Err(expire(error, attempts));
        }
        attempts += 1;
        let result = {
            let _scope = BudgetScope::enter(AttemptBudget {
                until: deadline.min(now + policy.attempt_timeout),
                clock: Arc::clone(clock),
            });
            attempt(attempts)
        };
        let error = match result {
            Ok(value) => {
                if attempts > 1 {
                    tracing::info!(
                        target: TARGET,
                        db = %db.display(),
                        operation = operation.as_str(),
                        thread_id,
                        attempts,
                        elapsed_ms = millis(clock.now().saturating_duration_since(started)),
                        "ownership registry operation succeeded after contention"
                    );
                }
                return Ok(value);
            }
            Err(error) if error.is_retryable() => error,
            Err(error) => return Err(error),
        };
        // The attempt's transaction is rolled back: its connection is gone.
        if interrupted() {
            return Err(interrupt(attempts));
        }
        let remaining = deadline.saturating_duration_since(clock.now());
        let Some(spare) = remaining
            .checked_sub(MIN_ATTEMPT)
            .filter(|spare| !spare.is_zero())
        else {
            return Err(expire(error, attempts));
        };
        if !sleep_unless(clock.as_ref(), jitter(backoff).min(spare), interrupted) {
            return Err(interrupt(attempts));
        }
        last_error = Some(error);
        backoff = backoff.saturating_mul(2).min(policy.max_backoff);
    }
}

/// Sleep for `duration` in slices; false if interrupted first.
fn sleep_unless(clock: &dyn Clock, duration: Duration, interrupted: &dyn Fn() -> bool) -> bool {
    let end = clock.now() + duration;
    loop {
        let left = end.saturating_duration_since(clock.now());
        if left.is_zero() {
            return true;
        }
        clock.sleep(left.min(SLEEP_SLICE));
        if interrupted() {
            return false;
        }
    }
}

/// Equal jitter: half the backoff plus a random share of the other half, so
/// contending writers do not retry in lockstep.
fn jitter(backoff: Duration) -> Duration {
    let mut bytes = [0; 8];
    let fraction = if getrandom::fill(&mut bytes).is_ok() {
        u64::from_le_bytes(bytes) as f64 / u64::MAX as f64
    } else {
        0.5
    };
    backoff / 2 + backoff.mul_f64(fraction / 2.0)
}

#[cfg(test)]
pub(super) mod virtual_time {
    //! A clock whose sleeps advance time instantly, for deterministic budgets.
    use std::sync::Mutex;

    use super::*;

    type Hook = Box<dyn FnMut(Duration) + Send>;

    pub(crate) struct VirtualClock {
        base: Instant,
        elapsed: Mutex<Duration>,
        on_sleep: Mutex<Option<Hook>>,
    }

    impl VirtualClock {
        pub(crate) fn new() -> Arc<Self> {
            Arc::new(Self {
                base: Instant::now(),
                elapsed: Mutex::new(Duration::ZERO),
                on_sleep: Mutex::new(None),
            })
        }

        /// Run `hook(elapsed)` after every sleep, on the sleeping thread.
        pub(crate) fn on_sleep(&self, hook: impl FnMut(Duration) + Send + 'static) {
            *self.on_sleep.lock().unwrap() = Some(Box::new(hook));
        }

        pub(crate) fn elapsed(&self) -> Duration {
            *self.elapsed.lock().unwrap()
        }

        pub(crate) fn advance(&self, duration: Duration) {
            *self.elapsed.lock().unwrap() += duration;
        }

        /// The current attempt's remaining budget, as its busy handler sees it.
        pub(crate) fn attempt_remaining(&self) -> Option<Duration> {
            ATTEMPT_BUDGET.with(|slot| {
                slot.borrow()
                    .as_ref()
                    .map(|budget| budget.until.saturating_duration_since(self.now()))
            })
        }

        pub(crate) fn shared(self: &Arc<Self>) -> Arc<dyn Clock> {
            Arc::clone(self) as Arc<dyn Clock>
        }
    }

    impl Clock for VirtualClock {
        fn now(&self) -> Instant {
            self.base + self.elapsed()
        }

        fn sleep(&self, duration: Duration) {
            self.advance(duration);
            let elapsed = self.elapsed();
            // Take the hook out so it may sleep on this clock itself.
            let hook = self.on_sleep.lock().unwrap().take();
            if let Some(mut hook) = hook {
                hook(elapsed);
                let mut slot = self.on_sleep.lock().unwrap();
                if slot.is_none() {
                    *slot = Some(hook);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::virtual_time::VirtualClock;
    use super::*;

    fn busy() -> RegistryError {
        RegistryError::Database {
            operation: DbOperation::Checkpoint,
            phase: super::super::DbPhase::Commit,
            attempt: 1,
            source: rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
                Some("database is locked".into()),
            ),
        }
    }

    /// Emulate SQLite waiting on a lock until the attempt budget runs out,
    /// through the same busy handler a real connection uses.
    fn wait_out_the_attempt() {
        let mut count = 0;
        while wait_within_budget(count) {
            count += 1;
        }
    }

    fn policy(deadline_ms: u64, attempt_ms: u64) -> RetryPolicy {
        RetryPolicy {
            deadline: Duration::from_millis(deadline_ms),
            attempt_timeout: Duration::from_millis(attempt_ms),
            initial_backoff: Duration::from_millis(25),
            max_backoff: Duration::from_millis(500),
        }
    }

    fn run<T>(
        clock: &Arc<VirtualClock>,
        policy: &RetryPolicy,
        interrupted: &dyn Fn() -> bool,
        attempt: impl FnMut(u32) -> Result<T, RegistryError>,
    ) -> Result<T, RegistryError> {
        retry_contention(
            Path::new("owners.db"),
            DbOperation::Checkpoint,
            "thread",
            policy,
            &clock.shared(),
            interrupted,
            attempt,
        )
    }

    #[test]
    fn default_policy_bounds_retries_near_ten_seconds() {
        let policy = RetryPolicy::default();
        assert_eq!(policy.deadline, Duration::from_secs(10));
        assert_eq!(policy.attempt_timeout, Duration::from_secs(2));
    }

    #[test]
    fn jitter_stays_within_half_to_full_backoff() {
        let backoff = Duration::from_millis(100);
        for _ in 0..100 {
            let wait = jitter(backoff);
            assert!(wait >= backoff / 2 && wait <= backoff, "{wait:?}");
        }
    }

    #[test]
    fn busy_delays_follow_sqlite_and_stop_at_the_budget() {
        let now = Instant::now();
        let until = now + Duration::from_millis(30);
        assert_eq!(busy_delay(0, now, until), Some(Duration::from_millis(1)));
        assert_eq!(busy_delay(11, now, until), Some(Duration::from_millis(30)));
        assert_eq!(busy_delay(500, now, until), Some(Duration::from_millis(30)));
        assert_eq!(busy_delay(0, until, until), None);
        assert_eq!(busy_delay(0, until + Duration::from_millis(1), until), None);
    }

    #[test]
    fn the_busy_handler_never_waits_outside_an_attempt() {
        assert!(!wait_within_budget(0));
    }

    #[test]
    fn statements_of_one_attempt_share_its_budget() {
        let clock = VirtualClock::new();
        let mut waits = Vec::new();
        let result: Result<(), _> = run(&clock, &policy(2000, 2000), &|| false, |_| {
            // BEGIN waits 1.5 s, then COMMIT waits for whatever is left.
            let start = clock.elapsed();
            let mut count = 0;
            while clock.elapsed() - start < Duration::from_millis(1500) {
                assert!(wait_within_budget(count));
                count += 1;
            }
            waits.push(clock.elapsed() - start);
            let commit = clock.elapsed();
            wait_out_the_attempt();
            waits.push(clock.elapsed() - commit);
            Err(busy())
        });
        let Err(RegistryError::Database { attempt: 1, .. }) = result else {
            panic!("one attempt, then the deadline: {result:?}");
        };
        assert!(waits[0] >= Duration::from_millis(1500), "{waits:?}");
        assert_eq!(
            waits[0] + waits[1],
            Duration::from_millis(2000),
            "{waits:?}"
        );
        assert_eq!(clock.elapsed(), Duration::from_millis(2000));
    }

    #[test]
    fn attempts_end_at_the_operation_deadline_and_none_starts_past_it() {
        let clock = VirtualClock::new();
        let mut budgets = Vec::new();
        let result: Result<(), _> = run(&clock, &policy(10_000, 2000), &|| false, |number| {
            budgets.push((number, clock.attempt_remaining().unwrap()));
            wait_out_the_attempt();
            Err(busy())
        });
        let Err(RegistryError::Database { attempt, .. }) = result else {
            panic!("contention must surface as the last database error: {result:?}");
        };
        assert_eq!(attempt as usize, budgets.len());
        assert!(budgets.len() >= 5, "{budgets:?}");
        assert_eq!(budgets[0], (1, Duration::from_millis(2000)));
        let (_, last) = *budgets.last().unwrap();
        assert!(last < Duration::from_millis(2000), "{budgets:?}");
        assert!(budgets.iter().all(|(_, budget)| *budget >= MIN_ATTEMPT));
        assert!(
            clock.elapsed() <= Duration::from_millis(10_000),
            "{:?}",
            clock.elapsed()
        );
        assert!(
            clock.elapsed() >= Duration::from_millis(10_000) - MIN_ATTEMPT,
            "{:?}",
            clock.elapsed()
        );
    }

    #[test]
    fn backoff_overshoot_does_not_start_another_attempt() {
        let policy = policy(300, 100);
        for elapsed_after_sleep in [
            policy.deadline - MIN_ATTEMPT + Duration::from_millis(1),
            policy.deadline,
            policy.deadline + Duration::from_millis(1),
        ] {
            let clock = VirtualClock::new();
            let sleeping_clock = Arc::downgrade(&clock);
            clock.on_sleep(move |elapsed| {
                sleeping_clock
                    .upgrade()
                    .unwrap()
                    .advance(elapsed_after_sleep - elapsed);
            });
            let mut calls = Vec::new();
            let result = run(&clock, &policy, &|| false, |number| {
                calls.push(number);
                if number == 1 { Err(busy()) } else { Ok(()) }
            });
            assert_eq!(
                calls,
                [1],
                "no attempt after backoff: {elapsed_after_sleep:?}"
            );
            let Err(RegistryError::Database {
                operation: DbOperation::Checkpoint,
                phase: super::super::DbPhase::Commit,
                attempt: 1,
                source: rusqlite::Error::SqliteFailure(code, Some(message)),
            }) = result
            else {
                panic!("the last DB error and attempt count must survive: {result:?}");
            };
            assert_eq!(code.extended_code, rusqlite::ffi::SQLITE_BUSY);
            assert_eq!(message, "database is locked");
            assert_eq!(clock.elapsed(), elapsed_after_sleep);
        }
    }

    #[test]
    fn an_interrupt_after_backoff_wins_over_an_exhausted_deadline() {
        let clock = VirtualClock::new();
        let policy = policy(300, 100);
        let sleeping_clock = Arc::downgrade(&clock);
        clock.on_sleep(move |_| {
            sleeping_clock.upgrade().unwrap().advance(policy.deadline);
        });
        let checked_after_sleep = AtomicBool::new(false);
        let mut calls = Vec::new();
        let result = run(
            &clock,
            &policy,
            &|| {
                // The last check inside sleep_unless sees no interrupt; it
                // arrives just before the next attempt's admission check.
                clock.elapsed() > policy.deadline
                    && checked_after_sleep.swap(true, Ordering::SeqCst)
            },
            |number| {
                calls.push(number);
                if number == 1 { Err(busy()) } else { Ok(()) }
            },
        );
        assert_eq!(calls, [1]);
        assert!(matches!(
            result,
            Err(RegistryError::Interrupted { attempts: 1, .. })
        ));
    }

    #[test]
    fn an_attempt_that_exhausts_the_deadline_is_the_last() {
        let clock = VirtualClock::new();
        let mut attempts = 0;
        let result: Result<(), _> = run(&clock, &policy(300, 2000), &|| false, |_| {
            attempts += 1;
            wait_out_the_attempt();
            Err(busy())
        });
        assert!(matches!(
            result,
            Err(RegistryError::Database { attempt: 1, .. })
        ));
        assert_eq!(attempts, 1);
        assert_eq!(clock.elapsed(), Duration::from_millis(300));
    }

    #[test]
    fn non_retryable_errors_return_after_one_attempt() {
        for error in [
            RegistryError::Ownership(super::super::OwnershipError::Fenced),
            RegistryError::Ownership(super::super::OwnershipError::Quiescing),
            RegistryError::Absent,
            RegistryError::Io(std::io::Error::other("disk")),
            RegistryError::Json(serde_json::from_str::<u8>("x").unwrap_err()),
            RegistryError::Sql(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_LOCKED),
                None,
            )),
            RegistryError::Sql(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_IOERR),
                None,
            )),
        ] {
            let mut attempts = 0;
            let mut error = Some(error);
            let result: Result<(), _> = run(
                &VirtualClock::new(),
                &RetryPolicy::default(),
                &|| false,
                |_| {
                    attempts += 1;
                    Err(error.take().expect("single attempt"))
                },
            );
            assert!(result.is_err());
            assert_eq!(attempts, 1);
        }
        assert!(
            RegistryError::Sql(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_LOCKED_SHAREDCACHE),
                None,
            ))
            .is_retryable()
        );
    }

    #[test]
    fn an_interrupt_before_the_first_attempt_runs_none() {
        let mut attempts = 0;
        let result: Result<(), _> = run(
            &VirtualClock::new(),
            &RetryPolicy::default(),
            &|| true,
            |_| {
                attempts += 1;
                Ok(())
            },
        );
        assert!(matches!(
            result,
            Err(RegistryError::Interrupted { attempts: 0, .. })
        ));
        assert_eq!(attempts, 0);
    }

    #[test]
    fn an_interrupt_stops_retrying_before_the_next_attempt() {
        let interrupted = AtomicBool::new(false);
        let mut attempts = 0;
        let result: Result<(), _> = run(
            &VirtualClock::new(),
            &RetryPolicy::default(),
            &|| interrupted.load(Ordering::SeqCst),
            |_| {
                attempts += 1;
                interrupted.store(true, Ordering::SeqCst);
                Err(busy())
            },
        );
        assert!(matches!(
            result,
            Err(RegistryError::Interrupted {
                operation: DbOperation::Checkpoint,
                attempts: 1
            })
        ));
        assert_eq!(attempts, 1);
    }

    #[test]
    fn an_interrupt_wins_over_an_exhausted_deadline() {
        let clock = VirtualClock::new();
        let interrupted = AtomicBool::new(false);
        let result: Result<(), _> = run(
            &clock,
            &policy(2000, 2000),
            &|| interrupted.load(Ordering::SeqCst),
            |_| {
                // Stop arrives during the attempt's last lock wait, which
                // then also exhausts the operation deadline.
                interrupted.store(true, Ordering::SeqCst);
                wait_out_the_attempt();
                Err(busy())
            },
        );
        assert_eq!(clock.elapsed(), Duration::from_millis(2000));
        assert!(
            matches!(
                result,
                Err(RegistryError::Interrupted {
                    operation: DbOperation::Checkpoint,
                    attempts: 1
                })
            ),
            "{result:?}"
        );
    }

    #[test]
    fn a_committed_attempt_wins_over_an_interrupt() {
        let interrupted = AtomicBool::new(false);
        let result = run(
            &VirtualClock::new(),
            &RetryPolicy::default(),
            &|| interrupted.load(Ordering::SeqCst),
            |_| {
                interrupted.store(true, Ordering::SeqCst);
                Ok(7)
            },
        );
        assert_eq!(result.unwrap(), 7);
    }

    #[test]
    fn an_interrupt_during_backoff_ends_it() {
        let clock = VirtualClock::new();
        let interrupted = Arc::new(AtomicBool::new(false));
        let mut policy = policy(10_000, 100);
        policy.initial_backoff = Duration::from_millis(400);
        policy.max_backoff = Duration::from_millis(400);
        let flag = Arc::clone(&interrupted);
        clock.on_sleep(move |elapsed| {
            if elapsed > Duration::from_millis(150) {
                flag.store(true, Ordering::SeqCst);
            }
        });
        let mut attempts = 0;
        let result: Result<(), _> = run(
            &clock,
            &policy,
            &|| interrupted.load(Ordering::SeqCst),
            |_| {
                attempts += 1;
                wait_out_the_attempt();
                Err(busy())
            },
        );
        assert!(matches!(
            result,
            Err(RegistryError::Interrupted { attempts: 1, .. })
        ));
        assert_eq!(attempts, 1);
        // Within one backoff slice of the interrupt.
        assert!(clock.elapsed() <= Duration::from_millis(150) + SLEEP_SLICE);
    }
}
