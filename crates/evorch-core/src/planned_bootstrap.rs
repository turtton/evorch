//! Planned-only successor pipeline. The product entry inspects inherited FDs
//! before creating its inbox; no public restart command is enabled here.
use core_instance::{
    ConfigIdentity, InheritedHandoffProof, Instance,
    handoff::{
        HandoffEntry, ShutdownInbox,
        boot::{BootFailure, BootWatchdog, PhysicalAdoptDeadline},
        receipt::{BootPhase, RestartBootReceipt},
    },
};
use runtime::{AgentRuntime, RestartAdoptionFailure};
use std::{
    os::unix::net::UnixListener,
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};
use storage::recovery::restart::{RestartMutation, RestartPhase};
use workbench_host::bootstrap::{
    BootstrapError, BootstrapOptions, BootstrappedHost, planned::PlannedStorage,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BootStage {
    PhysicalAdopted,
    StorageOpened,
    Adopted,
    FirstOwnerClaimed,
    OwnersClaimed,
    HostComposed,
    ControlReady,
    SandboxProbed,
    Ready,
}
/// Measurements are diagnostic only, never authority or execution deadlines.
#[derive(Default)]
pub struct BootMeasurements(pub Vec<(BootStage, Duration)>);

pub struct PlannedBoot {
    guard: BootWatchdog,
    instance: Arc<Instance>,
    config: ConfigIdentity,
    receipt: RestartBootReceipt,
    started: Instant,
    entry_elapsed: Duration,
    measurements: BootMeasurements,
}
/// No producer or graph activation is available through this wrapper. Only the
/// control handler is exposed for binding; mutating commands remain Preparing.
pub struct PendingPlannedHost {
    host: BootstrappedHost,
    boot: PlannedBoot,
    writer: storage::StorageHandle,
    database: storage::StorageConfig,
    pub blocked_threads: Vec<String>,
}
impl PlannedBoot {
    /// Must run at the single-threaded entry after FD inspection. There is no
    /// LaunchGuard acquisition and an error is never permission for Cold.
    pub fn adopt_entry(
        config: ConfigIdentity,
        parent: &Path,
        entry: HandoffEntry,
    ) -> Result<(Self, InheritedHandoffProof), BootstrapError> {
        Self::adopt_entry_inner(config, parent, entry, |config, parent, entry, _| {
            Instance::adopt_handoff(config, parent, entry)
        })
    }
    /// Production endpoint policy, under the same physical-adopt deadline.
    pub fn adopt_default_entry(
        config: ConfigIdentity,
        entry: HandoffEntry,
    ) -> Result<(Self, InheritedHandoffProof), BootstrapError> {
        Self::adopt_entry_inner(config, Path::new("/tmp"), entry, |config, _, entry, _| {
            let socket = config.default_socket()?;
            let parent = socket
                .parent()
                .and_then(Path::parent)
                .ok_or_else(|| std::io::Error::other("invalid default IPC parent"))?;
            Instance::adopt_handoff(config, parent, entry)
        })
    }
    /// Feature-gated fixture seam inside the physical check, not product argv.
    #[cfg(feature = "bootstrap-test-support")]
    pub fn adopt_entry_with_physical_hook(
        config: ConfigIdentity,
        parent: &Path,
        entry: HandoffEntry,
        before_elf: impl FnOnce(&PhysicalAdoptDeadline) -> std::io::Result<()>,
    ) -> Result<(Self, InheritedHandoffProof), BootstrapError> {
        Self::adopt_entry_inner(config, parent, entry, |config, parent, entry, timer| {
            Instance::adopt_handoff_with_physical_hook(config, parent, entry, || before_elf(timer))
        })
    }
    fn adopt_entry_inner(
        config: ConfigIdentity,
        parent: &Path,
        entry: HandoffEntry,
        physical_adopt: impl FnOnce(
            &ConfigIdentity,
            &Path,
            HandoffEntry,
            &PhysicalAdoptDeadline,
        ) -> std::io::Result<(Instance, InheritedHandoffProof)>,
    ) -> Result<(Self, InheritedHandoffProof), BootstrapError> {
        let record = entry.record();
        let (deadline, restart_id, predecessor_epoch, successor_epoch) = (
            record.boot_deadline,
            record.restart_id.clone(),
            record.predecessor_epoch,
            record.successor_epoch,
        );
        let physical_deadline = PhysicalAdoptDeadline::start(deadline)?;
        let (instance, proof) = physical_adopt(&config, parent, entry, &physical_deadline)?;
        let instance = Arc::new(instance);
        let entry_elapsed = deadline.elapsed()?;
        let started = Instant::now();
        let guard = physical_deadline.into_watchdog(instance.clone())?;
        let mut receipt = RestartBootReceipt::restarting(
            &config,
            restart_id,
            predecessor_epoch,
            successor_epoch,
        )?;
        // An old file supplies only the diagnostic revision, never eligibility,
        // epochs, the fixed deadline, or the authority to claim an owner.
        if let Ok(Some(previous)) = config.read_restart_receipt()
            && previous.transaction_id == receipt.transaction_id
            && previous.config_identity == receipt.config_identity
            && previous.predecessor_epoch == receipt.predecessor_epoch
            && previous.successor_epoch == receipt.successor_epoch
        {
            receipt.revision = previous.revision;
        }
        receipt.confirmed_phase = None;
        let mut boot = Self {
            guard,
            instance,
            config,
            receipt,
            started,
            entry_elapsed,
            measurements: BootMeasurements::default(),
        };
        boot.mark(BootStage::PhysicalAdopted);
        Ok((boot, proof))
    }
    fn mark(&mut self, stage: BootStage) {
        let elapsed = self.entry_elapsed + self.started.elapsed();
        tracing::info!(
            ?stage,
            elapsed_us = elapsed.as_micros(),
            "planned bootstrap stage"
        );
        self.measurements.0.push((stage, elapsed));
    }
    pub fn observe_signals(&mut self, inbox: &ShutdownInbox, runtime: Option<&AgentRuntime>) {
        if let Some(failure) = self.guard.terminal_failure() {
            self.fail(failure, runtime);
        }
        match inbox.read_pending() {
            Ok(signals) => {
                for signal in signals {
                    if self.guard.shutdown_signal(signal) {
                        self.fail(self.guard.selected_failure(), runtime);
                    }
                }
            }
            Err(_) => self.fail(BootFailure::BeforeAdopt, runtime),
        }
    }
    /// Never normal shutdown, release, Stop or fallback. The independent guard
    /// holds lifetime and exits at the original deadline even if diagnostics hang.
    pub fn fail(&mut self, failure: BootFailure, runtime: Option<&AgentRuntime>) -> ! {
        self.guard.select_failure(failure);
        let selected = self.guard.selected_failure();
        eprintln!(
            "evorch-core: planned restart failed ({selected:?}, exit {}); \
             check the configuration and start the core explicitly; no cold fallback was attempted",
            selected.exit_code()
        );
        let phase = runtime
            .and_then(|runtime| runtime.adopted_restart_transaction().ok())
            .map(|_| BootPhase::Adopted);
        if self.receipt.failed(selected, phase).is_ok() {
            if self.config.write_restart_receipt(&self.receipt).is_err() {
                eprintln!("evorch-core: restart diagnostic file unavailable");
            }
            if let Some(runtime) = runtime
                && let Some(progress) = runtime
                    .with_storage_reader(|db| {
                        db.restart_boot_progress(&self.receipt.transaction_id)
                    })
                    .ok()
                    .flatten()
                    .flatten()
                && runtime
                    .record_restart_boot_receipt(&self.receipt, progress.restarting_at_ms)
                    .is_err()
            {
                eprintln!("evorch-core: restart database diagnostic unavailable");
            }
        }
        self.guard.exit_after_diagnostic()
    }
    pub async fn bootstrap(
        mut self,
        options: BootstrapOptions,
        proof: InheritedHandoffProof,
        inbox: &ShutdownInbox,
    ) -> PendingPlannedHost {
        let stage = self.open_storage(options, inbox).await;
        self.finish_bootstrap(stage, proof, inbox).await
    }
    /// The only pre-Adopt storage opening path; it installs the signal guard
    /// and deliberately omits ordinary startup maintenance.
    pub async fn open_storage(
        &mut self,
        options: BootstrapOptions,
        inbox: &ShutdownInbox,
    ) -> PlannedStorage {
        if self.guard.watch_shutdown(inbox).is_err() {
            self.fail(BootFailure::BeforeAdopt, None);
        }
        self.observe_signals(inbox, None);
        if std::fs::canonicalize(&options.config_dir).ok().as_deref()
            != Some(self.config.config_dir())
        {
            self.fail(BootFailure::BeforeAdopt, None);
        }
        let stage = match PlannedStorage::open_with_lease_wait(
            options,
            Duration::from_secs(core_instance::handoff::boot::BOOT_TIMEOUT_SECS),
        )
        .await
        {
            Ok(stage) => stage,
            Err(_) => self.fail(BootFailure::BeforeAdopt, None),
        };
        self.mark(BootStage::StorageOpened);
        stage
    }
    pub async fn finish_bootstrap(
        mut self,
        stage: PlannedStorage,
        mut proof: InheritedHandoffProof,
        inbox: &ShutdownInbox,
    ) -> PendingPlannedHost {
        if stage.database().db_path != self.config.config_dir().join("evorch-events.db") {
            self.fail(BootFailure::BeforeAdopt, None);
        }
        let runtime = stage.runtime().clone();
        self.observe_signals(inbox, Some(&runtime));
        if runtime
            .with_storage_reader(|db| db.restart_transaction(&self.receipt.transaction_id))
            .ok()
            .flatten()
            .flatten()
            .is_some_and(|tx| tx.metadata.old_epoch != self.receipt.predecessor_epoch)
        {
            self.fail(BootFailure::AdoptRejected, Some(&runtime));
        }
        let mut retry = tokio::time::interval(Duration::from_secs(1));
        retry.tick().await;
        loop {
            match runtime.adopt_restart_handoff(proof) {
                Ok(()) => break,
                Err(error) => {
                    let error = *error;
                    proof = error.proof;
                    match error.failure {
                        RestartAdoptionFailure::Rejected => {
                            self.fail(BootFailure::AdoptRejected, Some(&runtime))
                        }
                        RestartAdoptionFailure::Unconfirmed => self.guard.ack_unknown(),
                    }
                    // The proof and ID never change; the boot monitor, not this
                    // retry interval, enforces the ticket's original deadline.
                    retry.tick().await;
                    self.observe_signals(inbox, Some(&runtime));
                }
            }
        }
        self.guard.adopted();
        self.mark(BootStage::Adopted);
        self.observe_signals(inbox, Some(&runtime));
        let restart_id = self.receipt.transaction_id.clone();
        let claimed = match stage
            .claim_with_progress(&restart_id, || self.mark(BootStage::FirstOwnerClaimed))
        {
            Ok(claimed) => claimed,
            Err(_) => self.fail(BootFailure::AdoptedBeforeReady, Some(&runtime)),
        };
        self.mark(BootStage::OwnersClaimed);
        // The endpoint publishes the receipt's epoch and the host answers hello
        // from its projection: both must be the epoch the transaction fixed.
        if claimed.successor_epoch() != self.receipt.successor_epoch {
            self.fail(BootFailure::AdoptedBeforeReady, Some(&runtime));
        }
        let blocked_threads = claimed.blocked_threads.clone();
        let writer = claimed.storage_writer();
        let database = claimed.database().clone();
        self.observe_signals(inbox, Some(&runtime));
        let host = match BootstrappedHost::open_claimed_for_core(claimed).await {
            Ok(host) => host,
            Err(_) => self.fail(BootFailure::AdoptedBeforeReady, Some(&runtime)),
        };
        self.mark(BootStage::HostComposed);
        self.observe_signals(inbox, Some(&runtime));
        PendingPlannedHost {
            host,
            boot: self,
            writer,
            database,
            blocked_threads,
        }
    }
}
impl PendingPlannedHost {
    pub fn web_config(&self, enabled: bool) -> workbench_web::WebServerConfig {
        workbench_web::WebServerConfig {
            enabled: enabled || self.host.config.web.enabled,
            bind: ([127, 0, 0, 1], self.host.config.web.port).into(),
            allowed_hosts: self.host.config.web.allowed_hosts.clone(),
            allowed_origins: self.host.config.web.allowed_origins.clone(),
            ..Default::default()
        }
    }

    pub fn control_host(&self) -> Arc<std::sync::Mutex<workbench_host::CommandHost>> {
        self.host.host.clone()
    }
    pub fn bind_control(&self) -> std::io::Result<UnixListener> {
        self.boot.instance.bind()
    }
    pub fn fail_boot(&mut self, failure: BootFailure) -> ! {
        let runtime = self.host.runtime.clone();
        self.boot.fail(failure, Some(&runtime))
    }
    pub fn measurements(&self) -> &BootMeasurements {
        &self.boot.measurements
    }
    pub fn successor_epoch(&self) -> u64 {
        self.boot.receipt.successor_epoch
    }
    /// Call only after the Web listener (if enabled) has also bound. Graph/tool
    /// comparison and learning dispatch remain deferred for the Ready caller.
    pub async fn confirm_ready(
        mut self,
        listener: UnixListener,
        at_ms: u64,
        inbox: &ShutdownInbox,
    ) -> (
        BootstrappedHost,
        Arc<Instance>,
        UnixListener,
        BootMeasurements,
    ) {
        let runtime = self.host.runtime.clone();
        self.boot.observe_signals(inbox, Some(&runtime));
        let valid_listener = listener
            .local_addr()
            .ok()
            .and_then(|a| a.as_pathname().map(Path::to_path_buf))
            .as_deref()
            == Some(&self.boot.instance.socket);
        if !valid_listener {
            self.boot
                .fail(BootFailure::AdoptedBeforeReady, Some(&runtime));
        }
        let before = match runtime.adopted_restart_transaction() {
            Ok(tx) => tx,
            Err(_) => self
                .boot
                .fail(BootFailure::AdoptedBeforeReady, Some(&runtime)),
        };
        if before.ready_at_ms.is_none() {
            let _ = self.writer.mutate_restart(
                before.metadata.restart_id.clone(),
                Some(before.revision),
                RestartMutation::Ready { at_ms },
            );
        }
        let confirmed = storage::Database::open(&self.database)
            .and_then(|db| db.restart_transaction(&before.metadata.restart_id))
            .ok()
            .flatten();
        if confirmed.is_none_or(|tx| {
            tx.phase != RestartPhase::Adopted && tx.phase != RestartPhase::Completed
                || tx.ready_at_ms != before.ready_at_ms.or(Some(at_ms))
                || tx.metadata != before.metadata
        }) {
            self.boot
                .fail(BootFailure::AdoptedBeforeReady, Some(&runtime));
        }
        if let Err(error) = self
            .host
            .admin
            .audit()
            .reconcile(storage::admin_audit::AuditReconcile::Ready {
                restart_id: before.metadata.restart_id.clone(),
            })
            .await
        {
            tracing::warn!(
                ?error,
                "management restart audit Ready reconciliation deferred"
            );
        }
        if self
            .boot
            .instance
            .publish(self.boot.receipt.successor_epoch)
            .is_err()
        {
            self.boot
                .fail(BootFailure::AdoptedBeforeReady, Some(&runtime));
        }
        self.boot.observe_signals(inbox, Some(&runtime));
        self.boot.mark(BootStage::ControlReady);
        if self.host.probe_after_ready().await.is_err() {
            self.boot.fail(BootFailure::SandboxProbe, Some(&runtime));
        }
        self.boot.mark(BootStage::SandboxProbed);
        self.boot.observe_signals(inbox, Some(&runtime));
        let guard = self.boot.guard;
        match guard.ready() {
            Ok(instance) => {
                self.host.owner.enable_normal_shutdown();
                self.boot.measurements.0.push((
                    BootStage::Ready,
                    self.boot.entry_elapsed + self.boot.started.elapsed(),
                ));
                (self.host, instance, listener, self.boot.measurements)
            }
            Err(guard) => {
                self.boot.guard = guard;
                self.boot
                    .fail(BootFailure::AdoptedBeforeReady, Some(&runtime))
            }
        }
    }
}
