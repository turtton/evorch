//! Config-scoped, persistent budget for core crashes seen by the launcher.
//!
//! Every client of one config directory shares one ledger file, so a core that
//! keeps dying cannot be restarted in a loop by several GUI/CLI clients, and the
//! budget survives a client restart. All mutations require the
//! [`LaunchGuard`] (the existing exclusive election): two clients never decide
//! on the same budget state at once, and the loser of the election reconnects to
//! the winner's core instead of starting another one.
//!
//! What counts is evidence of an unclean death, not a start:
//! - a socket left behind while the lifetime lock is free (a clean exit drops
//!   `Instance`, which removes it; SIGKILL, abort and OOM do not), and
//! - a core the launcher spawned that exited (other than `AlreadyRunning`,
//!   exit 2) before the launcher saw it ready.
//!
//! Normal start/exit cycles, idle exit, planned restart (exec handoff keeps the
//! lifetime lock), `AlreadyRunning` merges, connecting and polling never count.
//! An exhausted budget or an unreadable ledger is an error with a manual
//! recovery reason; it is never retried silently nor reset automatically.
use core_instance::LaunchGuard;
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// Crashes allowed per [`CRASH_BUDGET_WINDOW`] and config directory. Reaching
/// this many refuses the next automatic start.
pub const CRASH_BUDGET_LIMIT: u32 = 3;
pub const CRASH_BUDGET_WINDOW: Duration = Duration::from_secs(5 * 60);

const LEDGER_NAME: &str = ".evorch-core.crash-budget";
const LEDGER_TMP_NAME: &str = ".evorch-core.crash-budget.tmp";
const LEDGER_VERSION: u32 = 1;
const MAX_LEDGER_BYTES: u64 = 64 * 1024;
/// Only the newest `CRASH_BUDGET_LIMIT` crashes can matter; keep some more.
const MAX_ENTRIES: usize = 64;

#[derive(Debug, Clone, thiserror::Error)]
pub enum BudgetError {
    #[error(
        "automatic core start refused: {used} core crashes within the last {} seconds (limit {limit}) \
         for this config directory; no core was started and nothing is retried automatically. \
         Find out why evorch-core keeps dying (its stderr), fix it, then start again in about \
         {} seconds. The record is {}",
        window.as_secs(),
        retry_after.as_secs() + 1,
        ledger.display()
    )]
    Exhausted {
        used: u32,
        limit: u32,
        window: Duration,
        retry_after: Duration,
        ledger: PathBuf,
    },
    #[error(
        "crash budget record {} cannot be trusted ({reason}); automatic core start refused and \
         the record was not modified. Check that no evorch-core is running for this config \
         directory, then remove the file to reset the budget",
        path.display()
    )]
    Ledger { path: PathBuf, reason: String },
}

pub struct CrashBudget;
impl CrashBudget {
    /// Decide whether the launcher may start a core now. The caller holds the
    /// launch guard and has seen the lifetime lock free. `unclean_exit_seen` is
    /// the stale-socket evidence of a previous core's crash; it is recorded
    /// once, before the caller removes the evidence.
    pub fn admit_start(launch: &LaunchGuard, unclean_exit_seen: bool) -> Result<(), BudgetError> {
        let store = Store::from(launch);
        let now = now_ms(&store.path)?;
        let mut ledger = store.load()?;
        let mut dirty = ledger.normalize(now);
        if unclean_exit_seen {
            ledger.record(now);
            dirty = true;
        }
        if dirty {
            store.save(&ledger)?;
        }
        match ledger.exhausted(now) {
            None => Ok(()),
            Some(e) => Err(BudgetError::Exhausted {
                used: e.used,
                limit: CRASH_BUDGET_LIMIT,
                window: CRASH_BUDGET_WINDOW,
                retry_after: Duration::from_millis(e.retry_after_ms),
                ledger: store.path,
            }),
        }
    }
    /// A core the launcher started exited before it was seen ready.
    pub fn record_failed_start(launch: &LaunchGuard) -> Result<(), BudgetError> {
        let store = Store::from(launch);
        let now = now_ms(&store.path)?;
        let mut ledger = store.load()?;
        ledger.normalize(now);
        ledger.record(now);
        store.save(&ledger)
    }
    /// Crashes counted in the current window (read-only snapshot for display).
    pub fn recent_crashes(config_dir: &Path) -> Result<u32, BudgetError> {
        let store = Store::new(config_dir);
        let now = now_ms(&store.path)?;
        let mut ledger = store.load()?;
        ledger.normalize(now);
        Ok(ledger.crashes_ms.len() as u32)
    }
}

fn now_ms(path: &Path) -> Result<u64, BudgetError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .map_err(|_| BudgetError::Ledger {
            path: path.to_path_buf(),
            reason: "system clock is before the UNIX epoch".into(),
        })
}

/// The decision logic. Pure: time is always an argument, so the window is
/// unit-tested with an injected clock and no I/O.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Ledger {
    version: u32,
    /// Unix milliseconds of each observed crash, oldest first.
    crashes_ms: Vec<u64>,
}
struct Exhausted {
    used: u32,
    retry_after_ms: u64,
}
impl Ledger {
    fn empty() -> Self {
        Self {
            version: LEDGER_VERSION,
            crashes_ms: Vec::new(),
        }
    }
    fn validate(&self) -> Result<(), String> {
        if self.version != LEDGER_VERSION {
            return Err(format!("unsupported version {}", self.version));
        }
        if self.crashes_ms.len() > MAX_ENTRIES {
            return Err("too many entries".into());
        }
        if self.crashes_ms.windows(2).any(|w| w[0] > w[1]) {
            return Err("entries are not in time order".into());
        }
        Ok(())
    }
    /// Drop expired entries. An entry stamped in the future (the clock stepped
    /// backwards) is re-stamped to `now`; otherwise it would count until the
    /// clock catches up. Returns whether anything changed (the caller persists).
    fn normalize(&mut self, now_ms: u64) -> bool {
        let window = CRASH_BUDGET_WINDOW.as_millis() as u64;
        let before = self.crashes_ms.clone();
        for t in &mut self.crashes_ms {
            *t = (*t).min(now_ms);
        }
        self.crashes_ms.retain(|t| now_ms - *t < window);
        self.crashes_ms != before
    }
    /// Assumes `normalize(now_ms)` ran.
    fn record(&mut self, now_ms: u64) {
        self.crashes_ms.push(now_ms);
        let excess = self.crashes_ms.len().saturating_sub(MAX_ENTRIES);
        self.crashes_ms.drain(..excess);
    }
    /// Assumes `normalize(now_ms)` ran.
    fn exhausted(&self, now_ms: u64) -> Option<Exhausted> {
        let used = self.crashes_ms.len();
        let limit = CRASH_BUDGET_LIMIT as usize;
        if used < limit {
            return None;
        }
        // Starts are allowed again once fewer than `limit` crashes remain.
        let last_to_expire = self.crashes_ms[used - limit];
        let window = CRASH_BUDGET_WINDOW.as_millis() as u64;
        Some(Exhausted {
            used: used as u32,
            retry_after_ms: (last_to_expire + window).saturating_sub(now_ms),
        })
    }
}

struct Store {
    dir: PathBuf,
    path: PathBuf,
}
impl Store {
    fn new(dir: &Path) -> Self {
        Self {
            dir: dir.to_path_buf(),
            path: dir.join(LEDGER_NAME),
        }
    }
    fn from(launch: &LaunchGuard) -> Self {
        Self::new(launch.config().config_dir())
    }
    fn anomaly(&self, reason: impl Into<String>) -> BudgetError {
        BudgetError::Ledger {
            path: self.path.clone(),
            reason: reason.into(),
        }
    }
    fn io(&self, what: &str, e: io::Error) -> BudgetError {
        self.anomaly(format!("{what}: {e}"))
    }
    fn load(&self) -> Result<Ledger, BudgetError> {
        let file = match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(&self.path)
        {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Ledger::empty()),
            Err(e) => return Err(self.io("cannot open", e)),
        };
        self.check(&file)?;
        let mut data = Vec::new();
        file.take(MAX_LEDGER_BYTES + 1)
            .read_to_end(&mut data)
            .map_err(|e| self.io("cannot read", e))?;
        if data.len() as u64 > MAX_LEDGER_BYTES {
            return Err(self.anomaly("file is too large"));
        }
        let ledger: Ledger = serde_json::from_slice(&data)
            .map_err(|e| self.anomaly(format!("not parseable: {e}")))?;
        ledger.validate().map_err(|r| self.anomaly(r))?;
        Ok(ledger)
    }
    fn check(&self, file: &File) -> Result<(), BudgetError> {
        let m = file.metadata().map_err(|e| self.io("cannot stat", e))?;
        let p = fs::symlink_metadata(&self.path).map_err(|e| self.io("cannot stat", e))?;
        if !m.is_file()
            || m.uid() != rustix::process::geteuid().as_raw()
            || m.mode() & 0o7777 != 0o600
            || m.nlink() != 1
            || m.dev() != p.dev()
            || m.ino() != p.ino()
        {
            return Err(self.anomaly("unsafe file (owner, mode, links or type)"));
        }
        Ok(())
    }
    /// Atomic replace: readers see the old or the new ledger, never a partial one.
    /// Callers hold the launch guard, so the fixed temporary name is exclusive.
    fn save(&self, ledger: &Ledger) -> Result<(), BudgetError> {
        let tmp = self.dir.join(LEDGER_TMP_NAME);
        match fs::remove_file(&tmp) {
            Ok(()) => (),
            Err(e) if e.kind() == io::ErrorKind::NotFound => (),
            Err(e) => return Err(self.io("cannot clear temporary file", e)),
        }
        let write = || -> io::Result<()> {
            let mut f = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(&tmp)?;
            f.write_all(&serde_json::to_vec(ledger).map_err(io::Error::other)?)?;
            f.sync_all()?;
            fs::rename(&tmp, &self.path)?;
            File::open(&self.dir)?.sync_all()
        };
        write().map_err(|e| {
            let _ = fs::remove_file(&tmp);
            self.io("cannot write", e)
        })
    }
}

#[cfg(test)]
mod tests {
    //! Window decisions with an injected clock and no I/O. Persistence,
    //! corruption and concurrency use real files in `tests/crash_budget.rs`.
    use super::*;

    const MIN: u64 = 60_000;

    fn crash(l: &mut Ledger, now: u64) {
        l.normalize(now);
        l.record(now);
    }
    fn refused(l: &mut Ledger, now: u64) -> Option<Exhausted> {
        l.normalize(now);
        l.exhausted(now)
    }

    #[test]
    fn third_crash_within_the_window_refuses_with_the_time_until_a_slot_frees() {
        let mut l = Ledger::empty();
        crash(&mut l, 0);
        crash(&mut l, MIN);
        assert!(refused(&mut l, MIN).is_none());
        crash(&mut l, 2 * MIN);
        let e = refused(&mut l, 3 * MIN).unwrap();
        assert_eq!(e.used, 3);
        assert_eq!(e.retry_after_ms, 2 * MIN);
    }

    #[test]
    fn a_slot_frees_exactly_when_the_oldest_crash_leaves_the_window() {
        let mut l = Ledger::empty();
        for t in [0, MIN, 2 * MIN] {
            crash(&mut l, t);
        }
        assert!(refused(&mut l, 5 * MIN - 1).is_some());
        assert!(refused(&mut l, 5 * MIN).is_none());
        assert_eq!(l.crashes_ms, [MIN, 2 * MIN]);
    }

    #[test]
    fn retry_after_waits_for_enough_expiries_when_crashes_exceed_the_limit() {
        let mut l = Ledger::empty();
        for t in [0, 1, 2, 3, 4] {
            crash(&mut l, t * MIN);
        }
        // Five crashes: two must expire before fewer than three remain.
        let e = refused(&mut l, 4 * MIN).unwrap();
        assert_eq!(e.used, 5);
        assert_eq!(e.retry_after_ms, 7 * MIN - 4 * MIN);
    }

    #[test]
    fn no_crash_means_no_refusal_however_often_the_core_started() {
        let mut l = Ledger::empty();
        for t in 0..100 {
            assert!(refused(&mut l, t * 1000).is_none());
        }
        assert!(l.crashes_ms.is_empty());
    }

    #[test]
    fn a_crash_stamped_in_the_future_ages_from_now_and_is_normalized_once() {
        let mut l = Ledger {
            version: LEDGER_VERSION,
            crashes_ms: vec![600 * MIN; 3],
        };
        // The clock stepped back ten hours. The entries are re-stamped to now...
        assert!(l.normalize(0));
        assert_eq!(l.crashes_ms, [0, 0, 0]);
        // ...so a second evaluation changes nothing and they expire normally.
        assert!(!l.normalize(MIN));
        assert_eq!(l.crashes_ms.len(), 3);
        assert!(l.normalize(5 * MIN));
        assert!(l.crashes_ms.is_empty());
    }

    #[test]
    fn the_ledger_stays_bounded_and_keeps_the_newest_crashes() {
        let mut l = Ledger::empty();
        for t in 0..(MAX_ENTRIES as u64 + 10) {
            l.record(t);
        }
        assert_eq!(l.crashes_ms.len(), MAX_ENTRIES);
        assert_eq!(*l.crashes_ms.last().unwrap(), MAX_ENTRIES as u64 + 9);
        assert!(l.validate().is_ok());
    }

    #[test]
    fn inconsistent_ledgers_are_rejected_rather_than_repaired() {
        let mut l = Ledger::empty();
        assert!(l.validate().is_ok());
        l.crashes_ms = vec![2, 1];
        assert!(l.validate().is_err(), "out of order");
        l.crashes_ms = vec![0; MAX_ENTRIES + 1];
        assert!(l.validate().is_err(), "too many");
        l = Ledger::empty();
        l.version = 2;
        assert!(l.validate().is_err());
    }
}
