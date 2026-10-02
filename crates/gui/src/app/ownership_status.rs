//! Display-only ownership observations. Never use this cache to authorize an operation.

use std::time::{Duration, Instant};

use runtime::ownership::{RegistryError, ThreadOwner};

const CONTENTION_GRACE: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum OwnershipSnapshot {
    Unowned,
    Owned { owner: ThreadOwner, writable: bool },
}

#[derive(Debug)]
pub(super) struct ProbeFailure {
    pub since: Instant,
    pub contention: bool,
    pub detail: String,
}

#[derive(Debug, Default)]
pub(super) struct OwnershipStatus {
    pub thread: Option<String>,
    pub snapshot: Option<OwnershipSnapshot>,
    pub failure: Option<ProbeFailure>,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct OwnershipDisplay {
    pub access: &'static str,
    pub warning: bool,
}

impl OwnershipStatus {
    pub fn select_thread(&mut self, thread: Option<&str>) -> bool {
        if self.thread.as_deref() == thread {
            return false;
        }
        *self = Self {
            thread: thread.map(str::to_owned),
            ..Self::default()
        };
        true
    }

    pub fn observe(
        &mut self,
        thread: &str,
        result: Result<OwnershipSnapshot, RegistryError>,
        now: Instant,
    ) {
        self.select_thread(Some(thread));
        match result {
            Ok(snapshot) => {
                self.snapshot = Some(snapshot);
                self.failure = None;
            }
            Err(RegistryError::Absent) => {
                self.snapshot = Some(OwnershipSnapshot::Unowned);
                self.failure = None;
            }
            Err(error) => {
                let contention = matches!(
                    &error,
                    RegistryError::Sql(rusqlite::Error::SqliteFailure(error, _))
                        if matches!(error.code, rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
                );
                self.failure = Some(ProbeFailure {
                    since: self.failure.as_ref().map_or(now, |failure| failure.since),
                    contention,
                    detail: error.to_string(),
                });
            }
        }
    }

    pub fn display(&self, now: Instant) -> OwnershipDisplay {
        OwnershipDisplay {
            access: match &self.snapshot {
                Some(OwnershipSnapshot::Owned { writable: true, .. }) => "write",
                Some(OwnershipSnapshot::Unowned | OwnershipSnapshot::Owned { .. }) => "read-only",
                None => "checking ownership",
            },
            warning: self.failure.as_ref().is_some_and(|failure| {
                !failure.contention
                    || now.saturating_duration_since(failure.since) >= CONTENTION_GRACE
            }),
        }
    }
}

#[cfg(test)]
#[path = "ownership_status_tests.rs"]
mod tests;
