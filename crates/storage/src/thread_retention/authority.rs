//! Scoped single-writer capability. No production bootstrap wiring in PR1.
use crate::{ExclusiveLeases, StorageError};
use std::path::Path;

/// Proof borrowed from exclusive leases, never a flag or a shared handle.
///
/// A lease cannot be downgraded while an authority is still used:
///
/// ```compile_fail
/// use std::time::Duration;
/// use storage::{ExclusiveLeases, thread_retention::AutoArchiveAuthority};
/// fn forbidden(leases: ExclusiveLeases) {
///     let authority = AutoArchiveAuthority::new(&leases);
///     let shared = leases.downgrade(Duration::ZERO);
///     drop(authority);
/// }
/// ```
pub struct AutoArchiveAuthority<'a> {
    leases: &'a ExclusiveLeases,
}
impl<'a> AutoArchiveAuthority<'a> {
    #[must_use]
    pub fn new(leases: &'a ExclusiveLeases) -> Self {
        Self { leases }
    }
    pub fn validate(&self, db: &Path, root: &Path) -> Result<(), StorageError> {
        self.leases.validate_archive_identity(db, root)
    }
}

/// Independent from the startup-only delete policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AutoArchivePolicy {
    pub after_days: u32,
}
impl Default for AutoArchivePolicy {
    fn default() -> Self {
        Self { after_days: 7 }
    }
}
impl AutoArchivePolicy {
    /// `now - age`: activity at or before it is old enough. `None` when automatic
    /// archive is off or the age does not fit; an unrepresentable cutoff never lets a
    /// family become due early.
    #[must_use]
    pub fn cutoff(self, now_ns: i64) -> Option<i64> {
        if self.after_days == 0 {
            return None;
        }
        let age = i64::from(self.after_days).checked_mul(super::NANOS_PER_DAY)?;
        Some(now_ns.checked_sub(age).unwrap_or(i64::MIN))
    }
    /// Inclusive 24-hour days. Unrepresentable deadlines never become due.
    #[must_use]
    pub fn due(self, last_ns: i64, now_ns: i64) -> bool {
        self.after_days != 0
            && i64::from(self.after_days)
                .checked_mul(super::NANOS_PER_DAY)
                .and_then(|days| last_ns.checked_add(days))
                .is_some_and(|deadline| now_ns >= deadline)
    }
}

/// Fixture owner cannot outlive the exclusive borrow. No lockable file escapes.
/// Its destructor joins the writer before releasing that borrow, even if only a
/// cloned StorageHandle is subsequently used.
///
/// ```compile_fail
/// use std::{path::Path, time::Duration};
/// use storage::{ExclusiveLeases, StorageConfig, thread_retention::ExclusiveFixtureStorage};
/// fn forbidden(config: StorageConfig, leases: ExclusiveLeases) {
///     let fixture = ExclusiveFixtureStorage::open(config, Path::new("root"), &leases).unwrap();
///     let handle = fixture.handle();
///     let _shared = leases.downgrade(Duration::ZERO);
///     let _ = handle.barrier();
/// }
/// ```
///
/// Closing the owner first allows the downgrade (a remaining handle is closed):
///
/// ```no_run
/// use std::{path::Path, time::Duration};
/// use storage::{ExclusiveLeases, StorageConfig, thread_retention::ExclusiveFixtureStorage};
/// fn allowed(config: StorageConfig, leases: ExclusiveLeases) {
///     let fixture = ExclusiveFixtureStorage::open(config, Path::new("root"), &leases).unwrap();
///     let handle = fixture.handle();
///     drop(fixture);
///     let _shared = leases.downgrade(Duration::ZERO);
///     let _ = handle.barrier();
/// }
/// ```
#[cfg(feature = "test-support")]
pub struct ExclusiveFixtureStorage<'a> {
    storage: Option<crate::Storage>,
    _leases: &'a ExclusiveLeases,
}
#[cfg(feature = "test-support")]
impl<'a> ExclusiveFixtureStorage<'a> {
    pub fn open(
        config: crate::StorageConfig,
        root: &Path,
        leases: &'a ExclusiveLeases,
    ) -> Result<Self, StorageError> {
        leases.validate_archive_identity(&config.db_path, root)?;
        Ok(Self {
            storage: Some(crate::Storage::open_with_lease(
                config,
                leases.archive_writer_lease()?,
            )?),
            _leases: leases,
        })
    }
    pub fn handle(&self) -> crate::StorageHandle {
        self.storage.as_ref().expect("fixture owns writer").handle()
    }
}

#[cfg(feature = "test-support")]
impl Drop for ExclusiveFixtureStorage<'_> {
    fn drop(&mut self) {
        if let Some(storage) = self.storage.take() {
            storage.close();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn inclusive_deadline_and_overflow_never_archive_early() {
        let p = AutoArchivePolicy::default();
        let end = 7 * super::super::NANOS_PER_DAY;
        assert!(!p.due(0, end - 1));
        assert!(p.due(0, end));
        assert!(!p.due(i64::MAX - 1, i64::MAX));
        assert!(
            !AutoArchivePolicy {
                after_days: u32::MAX
            }
            .due(0, i64::MAX)
        );
        assert!(!AutoArchivePolicy { after_days: 0 }.due(0, i64::MAX));
        assert_eq!(p.cutoff(end), Some(0));
        assert_eq!(p.cutoff(0), Some(-end));
        assert_eq!(p.cutoff(i64::MIN + 1), Some(i64::MIN));
        assert_eq!(
            AutoArchivePolicy {
                after_days: u32::MAX
            }
            .cutoff(0),
            None
        );
        assert_eq!(AutoArchivePolicy { after_days: 0 }.cutoff(0), None);
    }
}
