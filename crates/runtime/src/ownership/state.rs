use serde::{Deserialize, Serialize};

use super::{Lease, OwnerState};

#[derive(Debug, thiserror::Error)]
pub enum OwnershipError {
    #[error("owner or generation no longer matches")]
    Fenced,
    #[error("thread is not accepting new turns")]
    Quiescing,
    #[error("thread has an active turn or needs a checkpoint")]
    Active,
    #[error("owner is not claimable")]
    NotClaimable,
    #[error("generation exhausted")]
    GenerationExhausted,
    #[error("previous owner still responds or liveness could not be established")]
    OwnerResponsive,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadOwner {
    pub thread_id: String,
    pub lease: Lease,
    pub state: OwnerState,
    pub active_turn: bool,
    #[serde(default)]
    pub active_runs: std::collections::BTreeSet<String>,
}

impl ThreadOwner {
    pub const fn new(thread_id: String, lease: Lease) -> Self {
        Self {
            thread_id,
            lease,
            state: OwnerState::Running,
            active_turn: false,
            active_runs: std::collections::BTreeSet::new(),
        }
    }

    pub fn validate(&self, token: &Lease) -> Result<(), OwnershipError> {
        if self.lease.owner_id != token.owner_id || self.lease.generation != token.generation {
            return Err(OwnershipError::Fenced);
        }
        Ok(())
    }

    pub fn begin_turn(&mut self, token: &Lease, now_ms: u64) -> Result<(), OwnershipError> {
        self.validate(token)?;
        if self.state != OwnerState::Running || now_ms >= self.lease.expires_at {
            return Err(OwnershipError::Quiescing);
        }
        if self.active_turn {
            return Err(OwnershipError::Active);
        }
        self.active_turn = true;
        Ok(())
    }

    pub fn quiesce(&mut self, token: &Lease) -> Result<(), OwnershipError> {
        self.validate(token)?;
        match self.state {
            OwnerState::Running | OwnerState::Quiescing => {
                self.state = OwnerState::Quiescing;
                Ok(())
            }
            OwnerState::Suspect
            | OwnerState::Stale
            | OwnerState::Claimable
            | OwnerState::Released => Err(OwnershipError::Quiescing),
        }
    }

    pub fn checkpoint(&mut self, token: &Lease) -> Result<(), OwnershipError> {
        self.validate(token)?;
        if !self.active_runs.is_empty() {
            return Err(OwnershipError::Active);
        }
        self.active_turn = false;
        Ok(())
    }

    pub fn begin_run(
        &mut self,
        token: &Lease,
        run: &str,
        now_ms: u64,
    ) -> Result<(), OwnershipError> {
        self.validate(token)?;
        if self.state != OwnerState::Running || now_ms >= self.lease.expires_at {
            return Err(OwnershipError::Quiescing);
        }
        if self.active_runs.contains(run) || (self.active_turn && self.active_runs.is_empty()) {
            return Err(OwnershipError::Active);
        }
        self.active_runs.insert(run.into());
        self.active_turn = true;
        Ok(())
    }

    pub fn checkpoint_run(&mut self, token: &Lease, run: &str) -> Result<(), OwnershipError> {
        self.validate(token)?;
        if !self.active_runs.remove(run) {
            return Err(OwnershipError::Active);
        }
        self.active_turn = !self.active_runs.is_empty();
        Ok(())
    }

    pub fn release(&mut self, token: &Lease) -> Result<(), OwnershipError> {
        self.validate(token)?;
        if self.active_turn {
            return Err(OwnershipError::Active);
        }
        if self.state != OwnerState::Quiescing {
            return Err(OwnershipError::Quiescing);
        }
        self.state = OwnerState::Released;
        self.lease.expires_at = 0;
        Ok(())
    }

    pub fn claim(
        &mut self,
        expected: &Lease,
        owner_id: &str,
        now_ms: u64,
        grace_ms: u64,
    ) -> Result<(), OwnershipError> {
        self.validate(expected)?;
        if self.active_turn {
            return Err(OwnershipError::Active);
        }
        if self.state != OwnerState::Released
            && self.lease.observe(now_ms, grace_ms) != OwnerState::Stale
        {
            return Err(OwnershipError::NotClaimable);
        }
        let generation = self
            .lease
            .generation
            .checked_add(1)
            .ok_or(OwnershipError::GenerationExhausted)?;
        self.lease = Lease {
            owner_id: owner_id.into(),
            generation,
            expires_at: now_ms.saturating_add(5_000),
        };
        self.state = OwnerState::Running;
        Ok(())
    }
}
