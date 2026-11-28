//! Restricted successor composition. Storage is opened once; no retention,
//! scratch sweep, credential backend, provider, MCP or producer runs here.
use super::{BootstrapError, BootstrapOptions, components};
use event_bus::EventBus;
use runtime::{AgentRuntime, RunStore, WorkspaceSeam, compose::StartupRuntime};
use std::{sync::Arc, time::Duration};

pub struct PlannedStorage {
    pub(super) options: BootstrapOptions,
    pub(super) config: config::Config,
    pub(super) storage: storage::Storage,
    pub(super) storage_config: storage::StorageConfig,
    pub(super) leases: storage::SharedLeases,
    pub(super) writer_leases: Arc<storage::WriterLeases>,
    pub(super) seed: StartupRuntime,
    pub(super) bus: Arc<EventBus>,
    pub(super) collector: Arc<super::recovery_disposal::LiveGraphCollector>,
}
/// Constructible only after proof-backed Adopt and transaction/epoch validation.
pub struct AdoptedBootstrap {
    pub(super) storage: PlannedStorage,
    pub(super) scratch: Arc<runtime::scratch::ThreadScratch>,
    pub(super) scratch_archive: Arc<runtime::scratch::archive::ArchiveService>,
    successor_epoch: u64,
}
impl AdoptedBootstrap {
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
        Self::open_with_lease_wait(options, storage::WRITER_LEASE_WAIT).await
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
        super::prepare_state_root(&options.ownership_root)?;
        let db = options.config_dir.join("evorch-events.db");
        let root = options.ownership_root.clone();
        let (writer_leases, leases) = tokio::task::spawn_blocking(move || {
            let budget = wait.min(storage::WRITER_LEASE_WAIT);
            let started = std::time::Instant::now();
            let writer = Arc::new(storage::WriterLeases::acquire(&db, &root, budget)?);
            let leases =
                storage::shared_with_wait(&db, &root, budget.saturating_sub(started.elapsed()))?;
            Ok::<_, BootstrapError>((writer, leases))
        })
        .await??;
        let storage_config =
            components::storage_config(&config, options.config_dir.join("evorch-events.db"));
        let storage = storage::Storage::open_with_writer_lease(
            storage_config.clone(),
            leases.db_lease()?,
            writer_leases.db_lease(),
        )?;
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
        let collector = super::recovery_disposal::LiveGraphCollector::new(storage_config.clone());
        seed.runtime().install_graph_observer(collector.clone())?;
        Ok(Self {
            collector,
            options,
            config,
            storage,
            storage_config,
            leases,
            writer_leases,
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
    pub fn prepare_adopted(self, restart_id: &str) -> Result<AdoptedBootstrap, BootstrapError> {
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
        let scratch = Arc::new(runtime::scratch::ThreadScratch::for_ownership_root(
            &self.options.ownership_root,
        )?);
        let guard = self
            .runtime()
            .tombstone_guard()
            .ok_or("startup tombstone guard missing")?;
        let scratch_archive = Arc::new(runtime::scratch::archive::ArchiveService::new(
            &scratch,
            runtime::scratch::archive::authority(
                self.storage_config.clone(),
                self.options.ownership_root.clone(),
                self.options.config_dir.join("sidebar.json"),
                self.writer_leases.clone(),
                scratch.clone(),
            ),
        )?);
        scratch_archive.set_guard(guard);
        self.runtime().set_thread_scratch(scratch.clone());
        self.runtime().set_disposal_context(
            self.writer_leases.clone(),
            self.options.ownership_root.clone(),
            self.options.config_dir.join("sidebar.json"),
        );
        Ok(AdoptedBootstrap {
            storage: self,
            scratch,
            scratch_archive,
            successor_epoch,
        })
    }
}
