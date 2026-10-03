use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use super::{Lease, OwnerState, OwnershipError, Registry, RegistryError, ThreadOwner};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnerPermit {
    pub registry_path: PathBuf,
    pub thread_id: String,
    pub lease: Lease,
    pub run_id: Option<String>,
}

impl OwnerPermit {
    pub fn mutation_guard(&self) -> Result<Registry, RegistryError> {
        let registry = Registry::open_readonly(&self.registry_path)?;
        registry.guard_generation(self)?;
        Ok(registry)
    }

    /// Attempt a generation guard without waiting for a SQLite writer. None
    /// means contention, which callers must retry after releasing other guards;
    /// an error means authority could not be established and fails closed.
    pub fn try_mutation_guard(&self) -> Result<Option<Registry>, RegistryError> {
        let attempt = || {
            let registry = Registry::open_readonly_nonblocking(&self.registry_path)?;
            registry.guard_generation(self)?;
            Ok(registry)
        };
        match attempt() {
            Ok(registry) => Ok(Some(registry)),
            Err(RegistryError::Sql(error))
                if matches!(
                    error.sqlite_error_code(),
                    Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
                ) =>
            {
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }
    pub fn validate_generation(&self) -> Result<(), RegistryError> {
        self.mutation_guard().map(drop)
    }
    pub fn begin_turn(&self) -> Result<(), RegistryError> {
        Registry::open_existing(&self.registry_path)?.update(
            &self.thread_id,
            |owner| match self.run_id.as_deref() {
                Some(run) => owner.begin_run(&self.lease, run, now_ms()),
                None => owner.begin_turn(&self.lease, now_ms()),
            },
        )?;
        Ok(())
    }

    pub fn validate_mutation(&self) -> Result<(), RegistryError> {
        self.validate_mutation_with_clock(now_ms)
    }

    pub(super) fn validate_mutation_with_clock(
        &self,
        clock: impl Fn() -> u64,
    ) -> Result<(), RegistryError> {
        let owner = Registry::open_readonly(&self.registry_path)?.attach(&self.thread_id)?;
        self.validate_active(&owner)?;
        if owner.lease.expires_at > clock() {
            return Ok(());
        }

        // Keep the usual probe read-only. A delayed heartbeat needs a write,
        // with authority rechecked under the same lock as the renewal so a
        // claim or handoff committed since the read always fences this permit.
        Registry::open_existing(&self.registry_path)?
            .update(&self.thread_id, |owner| {
                self.validate_active(owner)?;
                let now = clock();
                if owner.lease.expires_at <= now {
                    owner.heartbeat(&self.lease, now)?;
                }
                Ok(())
            })
            .inspect_err(|error| {
                tracing::warn!(
                    thread_id = %self.thread_id,
                    owner_id = %self.lease.owner_id,
                    generation = self.lease.generation,
                    %error,
                    "owner permit lease renewal failed"
                );
            })?;
        Ok(())
    }

    fn validate_active(&self, owner: &ThreadOwner) -> Result<(), OwnershipError> {
        owner.validate(&self.lease)?;
        if !owner.active_turn {
            return Err(OwnershipError::NotClaimable);
        }
        if self
            .run_id
            .as_ref()
            .is_some_and(|run| !owner.active_runs.contains(run))
        {
            return Err(OwnershipError::Active);
        }
        match owner.state {
            OwnerState::Running | OwnerState::Quiescing => Ok(()),
            OwnerState::Suspect
            | OwnerState::Stale
            | OwnerState::Claimable
            | OwnerState::Released => Err(OwnershipError::NotClaimable),
        }
    }

    pub fn checkpoint(&self, messages: &[providers::Message]) -> Result<(), RegistryError> {
        let mut registry = Registry::open_existing(&self.registry_path)?;
        let owner = registry.attach(&self.thread_id)?;
        owner.validate(&self.lease)?;
        if self
            .run_id
            .as_ref()
            .is_some_and(|run| !owner.active_runs.contains(run))
        {
            return Ok(());
        }
        registry.checkpoint_permit(self, messages)?;
        Ok(())
    }
}

pub(super) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}
