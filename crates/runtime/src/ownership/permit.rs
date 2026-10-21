use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use super::observe::DbOperation;
use super::registry::{ATTEMPT_TIMEOUT, Access, Wait};
use super::retry::{Clock, RetryPolicy, retry_contention, system_clock};
use super::{Lease, OwnerState, OwnershipError, Registry, RegistryError, ThreadOwner};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnerPermit {
    pub registry_path: PathBuf,
    pub thread_id: String,
    pub lease: Lease,
    pub run_id: Option<String>,
}

impl OwnerPermit {
    pub(crate) fn prepare_child<T>(
        &self,
        thread_id: &str,
        prepare: impl FnOnce() -> Result<T, String>,
    ) -> Result<(Self, T), String> {
        let (child, prepared) = Registry::open_existing(&self.registry_path)
            .and_then(|mut registry| {
                registry.prepare_child(self, thread_id, now_ms(), || {
                    prepare().map_err(RegistryError::Preparation)
                })
            })
            .map_err(|error| error.to_string())?;
        Ok((
            Self {
                registry_path: self.registry_path.clone(),
                thread_id: child.thread_id,
                lease: child.lease,
                run_id: None,
            },
            prepared,
        ))
    }

    pub fn mutation_guard(&self) -> Result<Registry, RegistryError> {
        let mut registry = Registry::open_attempt(
            &self.registry_path,
            Access::ReadOnly,
            Wait::Timeout(ATTEMPT_TIMEOUT),
            DbOperation::Guard,
            &self.thread_id,
            1,
        )?;
        registry.guard_generation(self)?;
        Ok(registry)
    }

    /// Attempt a generation guard without waiting for a SQLite writer. None
    /// means contention, which callers must retry after releasing other guards;
    /// an error means authority could not be established and fails closed.
    pub fn try_mutation_guard(&self) -> Result<Option<Registry>, RegistryError> {
        let attempt = || {
            let mut registry = Registry::open_readonly_nonblocking(&self.registry_path)?;
            registry.guard_generation(self)?;
            Ok::<_, RegistryError>(registry)
        };
        match attempt() {
            Ok(registry) => Ok(Some(registry)),
            Err(error) if error.is_contention() => Ok(None),
            Err(error) => Err(error),
        }
    }
    /// Check generation with bounded contention retries, dropping each read
    /// transaction before backoff. This is only a preflight: the eventual
    /// owner write must still revalidate authority inside its transaction.
    /// Blocks the calling thread; async callers use the blocking-pool variant.
    pub fn validate_generation(&self) -> Result<(), RegistryError> {
        self.validate_generation_with(&RetryPolicy::default(), &|| false)
    }

    pub fn validate_generation_with(
        &self,
        policy: &RetryPolicy,
        interrupted: &dyn Fn() -> bool,
    ) -> Result<(), RegistryError> {
        self.validate_generation_on(policy, &system_clock(), interrupted)
    }

    pub(super) fn validate_generation_on(
        &self,
        policy: &RetryPolicy,
        clock: &std::sync::Arc<dyn Clock>,
        interrupted: &dyn Fn() -> bool,
    ) -> Result<(), RegistryError> {
        retry_contention(
            &self.registry_path,
            DbOperation::Guard,
            &self.thread_id,
            policy,
            clock,
            interrupted,
            |attempt| {
                let mut registry = Registry::open_attempt(
                    &self.registry_path,
                    Access::ReadOnly,
                    Wait::Budget,
                    DbOperation::Guard,
                    &self.thread_id,
                    attempt,
                )?;
                registry.guard_generation(self)
            },
        )
    }

    /// Retry a short generation preflight off-thread. A contended read is not
    /// fencing; only a successful read can establish (or reject) authority.
    pub(crate) async fn validate_generation_off_thread(
        &self,
        policy: RetryPolicy,
        interrupted: impl Fn() -> bool + Send + 'static,
    ) -> Result<(), RegistryError> {
        let permit = self.clone();
        join_blocking(tokio::task::spawn_blocking(move || {
            permit.validate_generation_with(&policy, &interrupted)
        }))
        .await
    }
    /// Begin a turn, retrying registry contention within the default policy.
    /// Blocks the calling thread; async code uses [`Self::begin_turn_off_thread`].
    pub fn begin_turn(&self) -> Result<(), RegistryError> {
        self.begin_turn_with(&RetryPolicy::default(), &|| false)
    }

    /// Retrying is safe: each attempt re-reads the owner in a new IMMEDIATE
    /// transaction and applies the same pure transition, and a busy attempt
    /// (including a busy COMMIT) commits nothing. Should a previous attempt
    /// have committed after all, the run is already active and `begin_run`
    /// rejects it instead of opening a second turn.
    pub fn begin_turn_with(
        &self,
        policy: &RetryPolicy,
        interrupted: &dyn Fn() -> bool,
    ) -> Result<(), RegistryError> {
        self.begin_turn_on(policy, &system_clock(), interrupted)
    }

    pub(super) fn begin_turn_on(
        &self,
        policy: &RetryPolicy,
        clock: &std::sync::Arc<dyn Clock>,
        interrupted: &dyn Fn() -> bool,
    ) -> Result<(), RegistryError> {
        retry_contention(
            &self.registry_path,
            DbOperation::BeginTurn,
            &self.thread_id,
            policy,
            clock,
            interrupted,
            |attempt| {
                let mut registry = self.open_write_attempt(DbOperation::BeginTurn, attempt)?;
                registry.update_as(DbOperation::BeginTurn, &self.thread_id, |owner| match self
                    .run_id
                    .as_deref()
                {
                    Some(run) => owner.begin_run(&self.lease, run, now_ms()),
                    None => owner.begin_turn(&self.lease, now_ms()),
                })
            },
        )
        .map(drop)
    }

    /// [`Self::begin_turn_with`] on the blocking pool, so contention backoff
    /// never stalls an async worker. `interrupted` stops further attempts.
    pub(crate) async fn begin_turn_off_thread(
        &self,
        policy: RetryPolicy,
        interrupted: impl Fn() -> bool + Send + 'static,
    ) -> Result<(), RegistryError> {
        let permit = self.clone();
        join_blocking(tokio::task::spawn_blocking(move || {
            permit.begin_turn_with(&policy, &interrupted)
        }))
        .await
    }

    /// A connection whose lock waits share the current attempt's budget.
    fn open_write_attempt(
        &self,
        operation: DbOperation,
        attempt: u32,
    ) -> Result<Registry, RegistryError> {
        Registry::open_attempt(
            &self.registry_path,
            Access::ReadWrite,
            Wait::Budget,
            operation,
            &self.thread_id,
            attempt,
        )
    }

    pub fn validate_mutation(&self) -> Result<(), RegistryError> {
        self.validate_mutation_with_clock(now_ms)
    }

    pub(super) fn validate_mutation_with_clock(
        &self,
        clock: impl Fn() -> u64,
    ) -> Result<(), RegistryError> {
        let owner = Registry::open_attempt(
            &self.registry_path,
            Access::ReadOnly,
            Wait::Timeout(ATTEMPT_TIMEOUT),
            DbOperation::Validate,
            &self.thread_id,
            1,
        )?
        .attach_as(DbOperation::Validate, &self.thread_id)?;
        self.validate_active(&owner)?;
        if owner.lease.expires_at > clock() {
            return Ok(());
        }

        // Keep the usual probe read-only. A delayed heartbeat needs a write,
        // with authority rechecked under the same lock as the renewal so a
        // claim or handoff committed since the read always fences this permit.
        Registry::open_attempt(
            &self.registry_path,
            Access::ReadWrite,
            Wait::Timeout(ATTEMPT_TIMEOUT),
            DbOperation::Validate,
            &self.thread_id,
            1,
        )?
        .update_as(DbOperation::Validate, &self.thread_id, |owner| {
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

    /// Compatibility checkpoint: messages are deliberately not persisted.
    /// Retrying registry contention within the default
    /// policy. Blocks the calling thread; async code uses
    /// [`Self::checkpoint_off_thread`].
    pub fn checkpoint(&self, messages: &[providers::Message]) -> Result<(), RegistryError> {
        self.checkpoint_with(messages, &RetryPolicy::default(), &|| false)
    }

    pub fn checkpoint_with(
        &self,
        _messages: &[providers::Message],
        policy: &RetryPolicy,
        interrupted: &dyn Fn() -> bool,
    ) -> Result<(), RegistryError> {
        self.checkpoint_metadata_with(
            if self.run_id.is_some() {
                super::CheckpointBoundary::TurnEnd
            } else {
                super::CheckpointBoundary::Idle
            },
            super::SnapshotReference::none(super::SnapshotNoneReason::Skipped),
            policy,
            interrupted,
        )
    }

    pub fn checkpoint_metadata(
        &self,
        boundary: super::CheckpointBoundary,
        snapshot: super::SnapshotReference,
    ) -> Result<(), RegistryError> {
        self.checkpoint_metadata_with(boundary, snapshot, &RetryPolicy::default(), &|| false)
    }

    pub fn checkpoint_metadata_with(
        &self,
        boundary: super::CheckpointBoundary,
        snapshot: super::SnapshotReference,
        policy: &RetryPolicy,
        interrupted: &dyn Fn() -> bool,
    ) -> Result<(), RegistryError> {
        self.checkpoint_payload(
            &super::checkpoint::CheckpointInput { boundary, snapshot },
            policy,
            &system_clock(),
            interrupted,
        )
    }

    /// The payload is serialized once, before any write transaction, and
    /// reused by later attempts. Each attempt validates the permit again
    /// inside its own transaction.
    pub(super) fn checkpoint_payload<P: serde::Serialize + ?Sized>(
        &self,
        messages: &P,
        policy: &RetryPolicy,
        clock: &std::sync::Arc<dyn Clock>,
        interrupted: &dyn Fn() -> bool,
    ) -> Result<(), RegistryError> {
        let mut payload: Option<String> = None;
        retry_contention(
            &self.registry_path,
            DbOperation::Checkpoint,
            &self.thread_id,
            policy,
            clock,
            interrupted,
            |attempt| {
                let mut registry = self.open_write_attempt(DbOperation::Checkpoint, attempt)?;
                // This read only fences early; it never grants the write below.
                // Terminal references can refresh a run already checkpointed.
                let owner = registry.attach_as(DbOperation::Checkpoint, &self.thread_id)?;
                owner.validate(&self.lease)?;
                let serialized = match &mut payload {
                    Some(serialized) => serialized,
                    slot => slot.insert(registry.serialize_checkpoint(self, messages)?),
                };
                registry.checkpoint_serialized(self, serialized).map(drop)
            },
        )
    }

    /// [`Self::checkpoint_with`] on the blocking pool. `interrupted` stops
    /// further attempts; nothing is committed by an interrupted checkpoint.
    pub(crate) async fn checkpoint_off_thread(
        &self,
        boundary: super::CheckpointBoundary,
        snapshot: super::SnapshotReference,
        policy: RetryPolicy,
        interrupted: impl Fn() -> bool + Send + 'static,
    ) -> Result<(), RegistryError> {
        let permit = self.clone();
        join_blocking(tokio::task::spawn_blocking(move || {
            permit.checkpoint_metadata_with(boundary, snapshot, &policy, &interrupted)
        }))
        .await
    }
}

async fn join_blocking<T>(
    task: tokio::task::JoinHandle<Result<T, RegistryError>>,
) -> Result<T, RegistryError> {
    match task.await {
        Ok(result) => result,
        Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
        Err(error) => Err(RegistryError::Io(std::io::Error::other(format!(
            "ownership registry worker did not finish: {error}"
        )))),
    }
}

pub(super) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}
