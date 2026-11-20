//! Restricted successor composition. Storage is opened once; no retention,
//! scratch sweep, credential backend, provider, MCP or producer runs here.
use super::{BootstrapError, BootstrapOptions, components};
use event_bus::EventBus;
use runtime::{
    AgentRuntime, RunStore, WorkspaceSeam, compose::StartupRuntime, ownership::OwnerHost,
};
use std::{sync::Arc, time::Duration};

pub struct PlannedStorage {
    pub(super) options: BootstrapOptions,
    pub(super) config: config::Config,
    pub(super) storage: storage::Storage,
    pub(super) storage_config: storage::StorageConfig,
    pub(super) leases: storage::SharedLeases,
    pub(super) seed: StartupRuntime,
    pub(super) bus: Arc<EventBus>,
}
/// Constructible only after proof-backed Adopt and a full inventory claim pass.
/// Blocked threads are retained, never taken by an ordinary claim fallback.
pub struct ClaimedBootstrap {
    pub(super) storage: PlannedStorage,
    pub(super) owner: Arc<OwnerHost>,
    pub blocked_threads: Vec<String>,
    successor_epoch: u64,
}
impl ClaimedBootstrap {
    /// The epoch the adopted transaction fixed for this successor; the host
    /// composed from this claim starts its projection (hello, cursors) at it.
    pub fn successor_epoch(&self) -> u64 {
        self.successor_epoch
    }
    pub fn storage_writer(&self) -> storage::StorageHandle {
        self.storage.writer()
    }
    pub fn database(&self) -> &storage::StorageConfig {
        self.storage.database()
    }
}
impl PlannedStorage {
    pub async fn open(options: BootstrapOptions) -> Result<Self, BootstrapError> {
        Self::open_with_lease_wait(options, Duration::from_secs(30)).await
    }
    pub async fn open_with_lease_wait(
        options: BootstrapOptions,
        wait: Duration,
    ) -> Result<Self, BootstrapError> {
        let config = config::Config::load_strict(&config::LoadOptions {
            user_config_dir: Some(options.config_dir.clone()),
            project_dir: Some(options.project_root.clone()),
            read_env: false,
            ..Default::default()
        })?;
        let db = options.config_dir.join("evorch-events.db");
        let root = options.ownership_root.clone();
        // Shared-only: migration keeps its own existing schema/lease checks.
        let leases =
            tokio::task::spawn_blocking(move || storage::shared_with_wait(&db, &root, wait))
                .await??;
        let storage_config =
            components::storage_config(&config, options.config_dir.join("evorch-events.db"));
        let storage =
            storage::Storage::open_with_lease(storage_config.clone(), leases.db_lease()?)?;
        let bus = Arc::new(EventBus::new(4096));
        let seed = StartupRuntime::new(
            bus.clone(),
            WorkspaceSeam::production(options.project_root.clone())?,
            RunStore::open(&storage_config, storage.handle())?,
        );
        let tombstones = storage::Database::open(&storage_config)?.tombstones()?;
        seed.runtime()
            .set_tombstone_guard(runtime::thread_retention::TombstoneGuard::new(tombstones))
            .map_err(|_| "startup tombstone guard already set")?;
        Ok(Self {
            options,
            config,
            storage,
            storage_config,
            leases,
            seed,
            bus,
        })
    }
    pub fn runtime(&self) -> &AgentRuntime {
        self.seed.runtime()
    }
    pub fn writer(&self) -> storage::StorageHandle {
        self.storage.handle()
    }
    pub fn database(&self) -> &storage::StorageConfig {
        &self.storage_config
    }
    pub fn claim(self, restart_id: &str) -> Result<ClaimedBootstrap, BootstrapError> {
        self.claim_with_progress(restart_id, || {})
    }
    /// Called after the first successful limited claim (never an ordinary claim).
    pub fn claim_with_progress(
        self,
        restart_id: &str,
        first_claim: impl FnOnce(),
    ) -> Result<ClaimedBootstrap, BootstrapError> {
        // Mint evidence before even opening an owner worker.
        let evidence = self.runtime().adopted_restart_ownership_evidence()?;
        let tx = self.runtime().adopted_restart_transaction()?;
        if tx.metadata.restart_id != restart_id {
            return Err("adopted transaction mismatch".into());
        }
        let successor_epoch = tx.metadata.successor_epoch;
        if successor_epoch <= tx.metadata.old_epoch
            || successor_epoch == u64::MAX
            || tx.adopted_epoch != Some(successor_epoch)
        {
            return Err("adopted transaction epoch mismatch".into());
        }
        let owner = Arc::new(OwnerHost::open_planned(
            &self.options.ownership_root,
            self.config.ownership.clone(),
            self.bus.clone(),
        )?);
        let tombstones = storage::Database::open(&self.storage_config)?.tombstones()?;
        owner
            .set_tombstone_guard(runtime::thread_retention::TombstoneGuard::new(tombstones))
            .map_err(|_| "owner guard already set")?;
        let report =
            self.runtime()
                .claim_physical_bootstrap_owners(&owner, evidence, first_claim)?;
        let blocked_threads = report.blocked_threads;
        self.runtime().set_thread_scratch(owner.thread_scratch());
        Ok(ClaimedBootstrap {
            storage: self,
            owner,
            blocked_threads,
            successor_epoch,
        })
    }
}
