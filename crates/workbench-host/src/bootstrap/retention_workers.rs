//! Production workers are unchanged; fake-model bootstrap fixtures opt in.
use super::*;

pub(super) struct RetentionStartup {
    pub writer: storage::StorageHandle,
    pub storage: storage::StorageConfig,
    pub artifacts: Arc<runtime::artifacts::ArtifactStore>,
    pub ownership_root: PathBuf,
}

impl BootstrappedHost {
    pub(super) fn start_retention_workers(
        &mut self,
        startup: RetentionStartup,
    ) -> Result<(), BootstrapError> {
        // TestSeams constructors are test-support only. Production open never
        // reaches this branch, even when test-support is enabled by another target.
        #[cfg(any(test, feature = "test-support"))]
        if self.manual_retention_fixture {
            self.fixture_retention = Some(startup);
            return Ok(());
        }
        self.spawn_retention_workers(startup)
    }

    fn spawn_retention_workers(&mut self, startup: RetentionStartup) -> Result<(), BootstrapError> {
        let send_slot = Arc::new(std::sync::Mutex::new(()));
        let mut reclaim = runtime::thread_retention::ExecutorConfig::new(
            startup.writer.clone(),
            startup.storage.clone(),
            Some(startup.artifacts),
        );
        reclaim.send_slot = send_slot.clone();
        #[cfg(any(test, feature = "test-support"))]
        {
            reclaim.manual = self.manual_retention_fixture;
        }
        // Root contention disables only automatic archive/root cleanup, not DB reclaim.
        self.auto_archive = Some(crate::thread_retention::AutoArchiveOwner::start(
            Arc::downgrade(&self.host),
            crate::thread_retention::AutoArchiveCoordinator::new(
                startup.storage,
                startup.ownership_root,
                storage::thread_retention::AutoArchivePolicy {
                    after_days: self.config.storage.thread_retention.auto_archive_after_days,
                },
            )
            .with_send_slot(send_slot)
            .with_delete_policy(storage::thread_retention::ThreadRetentionPolicy {
                archived_delete_after_days: self
                    .config
                    .storage
                    .thread_retention
                    .archived_delete_after_days,
                ..Default::default()
            }),
            self.writer_leases.clone(),
            #[cfg(any(test, feature = "test-support"))]
            self.manual_retention_fixture,
        )?);
        reclaim.producer_gate = self
            .auto_archive
            .as_ref()
            .map(|owner| owner.producer_gate());
        self.reclaim = Some(runtime::thread_retention::ReclaimExecutor::spawn(reclaim)?);
        Ok(())
    }

    /// Explicit clock/tick for bootstrap fixtures; unavailable before activation.
    #[cfg(any(test, feature = "test-support"))]
    pub fn tick_retention_for_test(
        &self,
        now_ns: i64,
    ) -> Result<crate::thread_retention::RetentionReport, BootstrapError> {
        self.auto_archive
            .as_ref()
            .ok_or("retention producer not activated")?
            .tick_for_test(now_ns)
            .map_err(Into::into)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn tick_reclaim_for_test(
        &self,
    ) -> Result<runtime::thread_retention::ExecutorReport, BootstrapError> {
        self.reclaim
            .as_ref()
            .ok_or("reclaim producer not activated")?
            .tick_for_test()
            .map_err(Into::into)
    }

    /// Start the real retention producers in a fake-model composition fixture.
    /// Other fixtures drive bounded ticks directly, so a paused Tokio clock cannot
    /// race real std-worker I/O through the preparation deadline. Enable-only:
    /// no production setting or transport API can disable maintenance.
    #[cfg(any(test, feature = "test-support"))]
    pub fn enable_retention_producers_for_test(&mut self) -> Result<(), BootstrapError> {
        if let Some(startup) = self.fixture_retention.take() {
            self.spawn_retention_workers(startup)?;
        }
        Ok(())
    }
}
