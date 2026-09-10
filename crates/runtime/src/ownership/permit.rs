use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use super::{Lease, OwnerState, OwnershipError, Registry, RegistryError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnerPermit {
    pub registry_path: PathBuf,
    pub thread_id: String,
    pub lease: Lease,
    pub run_id: Option<String>,
}

impl OwnerPermit {
    pub fn begin_turn(&self) -> Result<(), RegistryError> {
        Registry::open(&self.registry_path)?.update(&self.thread_id, |owner| {
            match self.run_id.as_deref() {
                Some(run) => owner.begin_run(&self.lease, run, now_ms()),
                None => owner.begin_turn(&self.lease, now_ms()),
            }
        })?;
        Ok(())
    }

    pub fn validate_mutation(&self) -> Result<(), RegistryError> {
        let owner = Registry::open(&self.registry_path)?.attach(&self.thread_id)?;
        owner.validate(&self.lease)?;
        if !owner.active_turn || owner.lease.expires_at <= now_ms() {
            return Err(OwnershipError::NotClaimable.into());
        }
        if self.run_id.as_ref().is_some_and(|run| !owner.active_runs.contains(run)) {
            return Err(OwnershipError::Active.into());
        }
        match owner.state {
            OwnerState::Running | OwnerState::Quiescing => Ok(()),
            OwnerState::Suspect
            | OwnerState::Stale
            | OwnerState::Claimable
            | OwnerState::Released => Err(OwnershipError::NotClaimable.into()),
        }
    }

    pub fn checkpoint(&self, messages: &[providers::Message]) -> Result<(), RegistryError> {
        let owner = Registry::open(&self.registry_path)?.attach(&self.thread_id)?;
        owner.validate(&self.lease)?;
        if !owner.active_turn || self.run_id.as_ref().is_some_and(|run| !owner.active_runs.contains(run)) {
            return Ok(());
        }
        Registry::open(&self.registry_path)?.checkpoint_permit(self, messages)?;
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
