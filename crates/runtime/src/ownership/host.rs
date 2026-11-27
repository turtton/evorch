use std::os::unix::{
    fs::{MetadataExt, PermissionsExt},
    net::UnixListener,
};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex, MutexGuard, OnceLock,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::time::Duration;

use event_bus::{Event, EventBus, OwnershipAction, OwnershipEvent};

use super::{
    Lease, OwnerPermit, OwnerState, Registry, RegistryError, ThreadOwner, authority, ipc,
    permit::now_ms,
};
use crate::scratch::ThreadScratch;
use crate::thread_retention::TombstoneGuard;

#[cfg(test)]
#[path = "planned_tests.rs"]
mod planned_tests;

pub struct OwnerHost {
    root: PathBuf,
    owner_id: String,
    settings: config::OwnershipConfig,
    bus: Arc<EventBus>,
    // Reuse SQLite's page/statement caches for GUI probes, never ownership state.
    reader: Mutex<Registry>,
    // A GUI drain keeps generation guards valid until its queued writes finish.
    release_deferred: Arc<AtomicBool>,
    release_on_drop: AtomicBool,
    scratch: Arc<ThreadScratch>,
    /// Threads retention deleted; set once after the startup phase.
    tombstones: OnceLock<TombstoneGuard>,
    writer_leases: OnceLock<Arc<storage::WriterLeases>>,
    worker: Mutex<Option<WorkerControl>>,
    archive: super::scratch_archive::ArchiveService,
}

struct WorkerControl {
    stop: mpsc::Sender<()>,
    join: Option<std::thread::JoinHandle<()>>,
    failure: Option<String>,
    socket_identity: (u64, u64),
}

impl OwnerHost {
    /// Diagnostic only: no owner claim, generation mutation, or authority minting.
    pub fn disposal_cleanup_receipt(
        &self,
        operation_id: &str,
    ) -> Result<Option<super::DisposalOwnerReceipt>, RegistryError> {
        super::disposal_support::read(&self.root.join("owners.db"), operation_id)
    }
    pub fn open(
        root: &Path,
        settings: config::OwnershipConfig,
        bus: Arc<EventBus>,
    ) -> Result<Self, RegistryError> {
        Self::open_mode(root, settings, bus, true)
    }

    /// A planned successor neither sweeps scratch nor releases adopted owners
    /// on bootstrap failure. Normal shutdown is enabled only after Ready.
    pub fn open_planned(
        root: &Path,
        settings: config::OwnershipConfig,
        bus: Arc<EventBus>,
    ) -> Result<Self, RegistryError> {
        Self::open_mode(root, settings, bus, false)
    }
    pub fn retain_writer_leases(&self, leases: Arc<storage::WriterLeases>) {
        let _ = self.writer_leases.set(leases);
    }
    pub fn enable_normal_shutdown(&self) {
        self.release_on_drop.store(true, Ordering::SeqCst);
    }
    fn open_mode(
        root: &Path,
        settings: config::OwnershipConfig,
        bus: Arc<EventBus>,
        ordinary: bool,
    ) -> Result<Self, RegistryError> {
        Self::open_with_scratch(root, settings, bus, ordinary, None)
    }

    // Isolates the fixtures that exercise scratch without changing production selection.
    pub(crate) fn open_with_scratch(
        root: &Path,
        settings: config::OwnershipConfig,
        bus: Arc<EventBus>,
        ordinary: bool,
        scratch: Option<ThreadScratch>,
    ) -> Result<Self, RegistryError> {
        let mut random = [0u8; 16];
        getrandom::fill(&mut random).map_err(std::io::Error::other)?;
        let owner_id = random
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let socket = root.join(format!("{owner_id}.sock"));
        // Checked before any side effect: an overlong root must leave no
        // directory behind, and bind(2) alone reports a bare InvalidInput.
        check_socket_path(root, &socket)?;
        std::fs::create_dir_all(root)?;
        std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700))?;
        let listener = core_instance::bind_private_socket(&socket)?;
        listener.set_nonblocking(true)?;
        let metadata = std::fs::symlink_metadata(&socket)?;
        let socket_identity = (metadata.dev(), metadata.ino());
        let mut registry = Registry::open(&root.join("owners.db"))?;
        let reader = Mutex::new(Registry::open_readonly(&root.join("owners.db"))?);
        let scratch = Arc::new(match scratch {
            Some(scratch) => scratch,
            None => ThreadScratch::for_ownership_root(root)?,
        });
        if ordinary {
            sweep_scratch(&scratch, root, &owner_id, &mut registry);
        }
        let worker_scratch = Arc::clone(&scratch);
        let (stop, receiver) = mpsc::channel();
        let id = owner_id.clone();
        let worker_settings = settings.clone();
        let worker_bus = Arc::clone(&bus);
        let release_deferred = Arc::new(AtomicBool::new(false));
        let worker_deferred = Arc::clone(&release_deferred);
        let worker_db = root.join("owners.db");
        let archive = super::scratch_archive::ArchiveService::new(
            root.join("owners.db"),
            owner_id.clone(),
            Arc::clone(&scratch),
        )?;
        let worker = std::thread::spawn(move || {
            serve(
                registry,
                listener,
                receiver,
                Worker {
                    db: &worker_db,
                    id: &id,
                    settings: &worker_settings,
                    bus: &worker_bus,
                    release_deferred: &worker_deferred,
                    scratch: &worker_scratch,
                },
            )
        });
        Ok(Self {
            archive,
            root: root.into(),
            owner_id,
            settings,
            bus,
            reader,
            release_deferred,
            release_on_drop: AtomicBool::new(ordinary),
            scratch,
            tombstones: OnceLock::new(),
            writer_leases: OnceLock::new(),
            worker: Mutex::new(Some(WorkerControl {
                stop,
                join: Some(worker),
                failure: None,
                socket_identity,
            })),
        })
    }

    /// Refuse to start, claim or attach the threads retention deleted. Installed once,
    /// after the startup phase; returns the guard when one is already installed.
    pub fn set_tombstone_guard(&self, guard: TombstoneGuard) -> Result<(), TombstoneGuard> {
        self.tombstones.set(guard.clone())?;
        self.archive.set_guard(guard);
        Ok(())
    }

    fn deny_deleted(&self, thread_id: &str) -> Result<(), RegistryError> {
        match self.tombstones.get() {
            Some(guard) => Ok(guard.check_thread(thread_id)?),
            None => Ok(()),
        }
    }

    pub fn attach(&self, thread_id: &str) -> Result<ThreadOwner, RegistryError> {
        self.deny_deleted(thread_id)?;
        self.reader()?.attach(thread_id)
    }

    /// This process's owner identity, as recorded in leases it holds.
    pub fn owner_id(&self) -> &str {
        &self.owner_id
    }

    pub fn start(&self, thread_id: &str) -> Result<OwnerPermit, RegistryError> {
        self.deny_deleted(thread_id)?;
        let mut owner = ThreadOwner::new(
            thread_id.into(),
            Lease {
                owner_id: self.owner_id.clone(),
                generation: 1,
                expires_at: now_ms().saturating_add(self.settings.lease_ms.get()),
            },
        );
        owner.settings = self.settings.clone();
        Registry::open_existing(&self.root.join("owners.db"))?.start(&owner)?;
        clean_scratch(
            &self.db(),
            &owner.thread_id,
            &owner.lease,
            &self.scratch,
            true,
        );
        emit(&self.bus, &owner, OwnershipAction::Claimed);
        Ok(self.permit(&owner))
    }

    pub fn claim(&self, expected: &ThreadOwner) -> Result<OwnerPermit, RegistryError> {
        self.deny_deleted(&expected.thread_id)?;
        let mut registry = Registry::open_existing(&self.root.join("owners.db"))?;
        let socket = socket_path(&self.root, &expected.lease.owner_id)?;
        let owner = ipc::claim_configured(
            &mut registry,
            ipc::ClaimRequest {
                thread_id: &expected.thread_id,
                expected: &expected.lease,
                previous_socket: &socket,
                owner_id: &self.owner_id,
                now_ms: now_ms(),
                grace_ms: self.settings.grace_ms.get(),
            },
            &self.settings,
        )?;
        authority::record_fenced(&self.db(), &expected.thread_id, &expected.lease);
        clean_scratch(
            &self.db(),
            &owner.thread_id,
            &owner.lease,
            &self.scratch,
            true,
        );
        emit(&self.bus, &owner, OwnershipAction::Claimed);
        Ok(self.permit(&owner))
    }

    pub fn owned_permit(&self, thread_id: &str) -> Result<OwnerPermit, RegistryError> {
        let owner = self.attach(thread_id)?;
        if owner.lease.owner_id != self.owner_id || owner.state != OwnerState::Running {
            return Err(super::OwnershipError::Fenced.into());
        }
        Ok(self.permit(&owner))
    }

    pub fn handoff(
        &self,
        permit: &OwnerPermit,
        successor: &Self,
    ) -> Result<OwnerPermit, RegistryError> {
        if permit.lease.owner_id != self.owner_id || self.root != successor.root {
            return Err(super::OwnershipError::Fenced.into());
        }
        let mut registry = Registry::open_existing(&self.root.join("owners.db"))?;
        let owner = registry.update(&permit.thread_id, |owner| {
            owner.quiesce(&permit.lease)?;
            owner.release(&permit.lease)?;
            owner.settings = successor.settings.clone();
            owner.claim(&permit.lease, &successor.owner_id, now_ms(), 0)
        })?;
        authority::record_fenced(&self.db(), &permit.thread_id, &permit.lease);
        clean_scratch(
            &self.db(),
            &owner.thread_id,
            &owner.lease,
            &self.scratch,
            false,
        );
        clean_scratch(
            &self.db(),
            &owner.thread_id,
            &owner.lease,
            &successor.scratch,
            true,
        );
        emit(&successor.bus, &owner, OwnershipAction::Handoff);
        Ok(successor.permit(&owner))
    }

    /// Stop new turns without releasing idle generations, including from the
    /// heartbeat worker. Call `release_ready` only after durable queues drain.
    /// Returns whether existing turns still need to checkpoint.
    pub fn begin_quiesce(&self) -> Result<bool, RegistryError> {
        self.release_deferred.store(true, Ordering::SeqCst);
        let mut registry = Registry::open_existing(&self.root.join("owners.db"))?;
        let mut active = false;
        for owner in registry.list()?.into_iter().filter(|owner| {
            owner.lease.owner_id == self.owner_id && owner.state != OwnerState::Released
        }) {
            // The GUI checks progress each frame. Already-quiescing owners need
            // no duplicate registry write or durable transition event.
            let changed = if owner.state == OwnerState::Quiescing {
                owner
            } else {
                let changed =
                    registry.update(&owner.thread_id, |state| state.quiesce(&owner.lease))?;
                emit(&self.bus, &changed, OwnershipAction::Quiescing);
                changed
            };
            active |= changed.active_turn;
        }
        Ok(active)
    }

    /// Release quiescing owners whose turns have checkpointed. Remaining active
    /// owners may be released by the heartbeat worker after their checkpoint.
    pub fn release_ready(&self) -> Result<bool, RegistryError> {
        // Keep the worker from competing with this explicit release pass.
        self.release_deferred.store(true, Ordering::SeqCst);
        let mut registry = Registry::open_existing(&self.root.join("owners.db"))?;
        let mut active = false;
        for owner in registry.list()?.into_iter().filter(|owner| {
            owner.lease.owner_id == self.owner_id && owner.state != OwnerState::Released
        }) {
            active |= owner.active_turn;
            if owner.state == OwnerState::Quiescing
                && !owner.active_turn
                && !owner.has_planned_hold()
            {
                let released =
                    registry.update(&owner.thread_id, |state| state.release(&owner.lease))?;
                authority::record_fenced(&self.db(), &owner.thread_id, &owner.lease);
                clean_scratch(
                    &self.db(),
                    &released.thread_id,
                    &released.lease,
                    &self.scratch,
                    false,
                );
                emit(&self.bus, &released, OwnershipAction::Released);
            }
        }
        self.release_deferred.store(false, Ordering::SeqCst);
        Ok(active)
    }

    /// Legacy quiesce-and-release operation for callers without a durable drain.
    pub fn quiesce(&self) -> Result<bool, RegistryError> {
        self.begin_quiesce()?;
        self.release_ready()
    }

    pub fn has_active_turns(&self) -> Result<bool, RegistryError> {
        Ok(self
            .reader()?
            .list()?
            .iter()
            .any(|owner| owner.lease.owner_id == self.owner_id && owner.active_turn))
    }

    /// The thread scratch directories this host empties on claim and deletes on release.
    pub fn thread_scratch(&self) -> Arc<ThreadScratch> {
        Arc::clone(&self.scratch)
    }

    /// Fresh, complete unreleased inventory, including idle threads. Never uses
    /// cached owner state: planned hold save/readback must observe owners.db.
    pub fn restart_inventory(&self) -> Result<Vec<ThreadOwner>, RegistryError> {
        Ok(Registry::open_readonly(&self.db())?
            .list()?
            .into_iter()
            .filter(|owner| {
                owner.lease.owner_id == self.owner_id && owner.state != OwnerState::Released
            })
            .collect())
    }

    /// Call after admission fencing and before Armed. Includes idle threads.
    pub fn hold_for_restart(
        &self,
        tx: &str,
        capture: &str,
        expected: &super::ExpectedActiveRuns,
    ) -> Result<Vec<super::PlannedHold>, RegistryError> {
        self.release_deferred.store(true, Ordering::SeqCst);
        Registry::open_existing(&self.db())?.hold_owned(&self.owner_id, tx, capture, expected)
    }

    /// Dedicated capture binding: live runs must be captured, while parked runs
    /// require a same-generation nonterminal owner checkpoint in the same DB TX.
    pub fn hold_captured_for_restart(
        &self,
        tx: &str,
        capture: &str,
        expected: &super::ExpectedCapturedRuns,
    ) -> Result<Vec<super::PlannedHold>, RegistryError> {
        self.release_deferred.store(true, Ordering::SeqCst);
        Registry::open_existing(&self.db())?.hold_captured_owned(
            &self.owner_id,
            tx,
            capture,
            expected,
        )
    }

    /// FailedClosed cleanup, independent of the reason for failure. Does not
    /// reopen producers, clear active runs, release ownership or delete scratch.
    pub fn clear_planned_hold(
        &self,
        hold: &super::PlannedHold,
    ) -> Result<ThreadOwner, RegistryError> {
        if hold.owner_id != self.owner_id {
            return Err(super::OwnershipError::PlannedOwnerBlocked.into());
        }
        self.release_deferred.store(true, Ordering::SeqCst);
        Registry::open_existing(&self.db())?.clear_planned_hold(hold)
    }

    /// Call immediately after Adopted, before other bootstrap work. open() has
    /// already started this host's worker; claiming renews the lease immediately
    /// and enrolls the successor generation in its heartbeat scan.
    /// Map PlannedOwnerBlocked to 06c OwnerBlocked, NOT a normal claim fallback.
    pub fn claim_planned(
        &self,
        hold: &super::PlannedHold,
        evidence: &dyn super::AdoptedOwnershipEvidence,
    ) -> Result<OwnerPermit, RegistryError> {
        self.deny_deleted(&hold.thread_id)?;
        let owner = ipc::claim_planned(
            &mut Registry::open_existing(&self.db())?,
            hold,
            &self.root,
            &self.owner_id,
            now_ms(),
            &self.settings,
            evidence,
        )?;
        authority::record_fenced(&self.db(), &hold.thread_id, &hold.lease());
        emit(&self.bus, &owner, OwnershipAction::Claimed);
        // Deliberately no reset/release: scratch remains byte-for-byte intact.
        Ok(self.permit(&owner))
    }

    /// Armed readback -> stop/join -> identity-checked unlink -> exec. No Drop,
    /// release or PID-liveness assumption. Idempotent after successful stopping.
    pub fn stop_worker_for_exec(&self) -> Result<(), RegistryError> {
        let archive_result = self.archive.stop().map_err(RegistryError::from);
        let owner_result = self.stop_owner_worker();
        let cleanup_result = sandbox::scratch::stop_scratch_cleanup().map_err(RegistryError::from);
        archive_result.and(owner_result).and(cleanup_result)
    }

    fn stop_owner_worker(&self) -> Result<(), RegistryError> {
        let mut slot = self
            .worker
            .lock()
            .map_err(|_| RegistryError::ReaderPoisoned)?;
        if let Some(worker) = slot.as_mut() {
            if let Some(error) = &worker.failure {
                return Err(RegistryError::Preparation(error.clone()));
            }
            let _ = worker.stop.send(());
            if let Some(join) = worker.join.take()
                && join.join().is_err()
            {
                let error = "owner worker panicked while joining".to_owned();
                worker.failure = Some(error.clone());
                return Err(RegistryError::Preparation(error));
            }
            let socket = socket_path(&self.root, &self.owner_id)?;
            let cleanup = match std::fs::symlink_metadata(&socket) {
                Ok(metadata) if (metadata.dev(), metadata.ino()) == worker.socket_identity => {
                    std::fs::remove_file(socket)
                }
                Ok(_) => Err(std::io::Error::other(
                    "owner socket identity changed; refusing unlink",
                )),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(error),
            };
            if let Err(error) = cleanup {
                worker.failure = Some(error.to_string());
                return Err(error.into());
            }
            *slot = None;
        }
        Ok(())
    }

    /// Restore only owner heartbeat/IPC after failed exec. Admission and run
    /// gates stay closed; call clear_planned_hold for every installed hold.
    pub fn restart_worker_failed_closed(&self) -> Result<(), RegistryError> {
        sandbox::scratch::resume_scratch_cleanup()?;
        let mut slot = self
            .worker
            .lock()
            .map_err(|_| RegistryError::ReaderPoisoned)?;
        if let Some(worker) = slot.as_ref() {
            if let Some(error) = &worker.failure {
                return Err(RegistryError::Preparation(error.clone()));
            }
            return Ok(());
        }
        self.release_deferred.store(true, Ordering::SeqCst);
        let mut registry = Registry::open_existing(&self.db())?;
        registry.heartbeat_owned(
            &self.owner_id,
            now_ms(),
            self.settings.heartbeat_ms.get(),
            false,
        )?;
        let socket = socket_path(&self.root, &self.owner_id)?;
        let listener = core_instance::bind_private_socket(&socket)?;
        listener.set_nonblocking(true)?;
        let metadata = std::fs::symlink_metadata(&socket)?;
        let socket_identity = (metadata.dev(), metadata.ino());
        let (stop, receiver) = mpsc::channel();
        let db = self.db();
        let id = self.owner_id.clone();
        let settings = self.settings.clone();
        let bus = Arc::clone(&self.bus);
        let deferred = Arc::clone(&self.release_deferred);
        let scratch = Arc::clone(&self.scratch);
        let join = std::thread::spawn(move || {
            serve(
                registry,
                listener,
                receiver,
                Worker {
                    db: &db,
                    id: &id,
                    settings: &settings,
                    bus: &bus,
                    release_deferred: &deferred,
                    scratch: &scratch,
                },
            )
        });
        *slot = Some(WorkerControl {
            stop,
            join: Some(join),
            failure: None,
            socket_identity,
        });
        Ok(())
    }

    /// Inject saved shared-sidebar state. Called only on the archive worker.
    pub fn set_scratch_archive_checker(&self, checker: Arc<super::ScratchArchiveChecker>) {
        self.archive.set_checker(checker);
    }
    /// Best effort hint after a successful archive save, outside its save lock.
    pub fn hint_scratch_archive(&self, thread: &str) {
        self.archive.hint(thread);
    }
    /// Call after producers have stopped, runs drained and ownership released.
    pub fn stop_scratch_cleanup(&self) -> Result<(), RegistryError> {
        let archive = self.archive.stop().map_err(RegistryError::from);
        let cleanup = sandbox::scratch::stop_scratch_cleanup().map_err(RegistryError::from);
        archive.and(cleanup)
    }
    #[cfg(any(test, feature = "idle-test-support"))]
    #[doc(hidden)]
    pub fn scratch_archive_barrier(&self) -> std::sync::mpsc::Receiver<()> {
        self.archive.barrier()
    }

    fn reader(&self) -> Result<MutexGuard<'_, Registry>, RegistryError> {
        self.reader
            .lock()
            .map_err(|_| RegistryError::ReaderPoisoned)
    }

    fn permit(&self, owner: &ThreadOwner) -> OwnerPermit {
        self.scratch
            .record_scratch_lease(&owner.thread_id, &owner.lease);
        OwnerPermit {
            registry_path: self.db(),
            thread_id: owner.thread_id.clone(),
            lease: owner.lease.clone(),
            run_id: None,
        }
    }

    fn db(&self) -> PathBuf {
        self.root.join("owners.db")
    }
}

/// Bytes an AF_UNIX path must stay below (`sizeof(sockaddr_un.sun_path)`,
/// terminating NUL included): 104 on macOS and the BSDs, 108 elsewhere.
pub(super) const SOCKET_PATH_LIMIT: usize = if cfg!(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly"
)) {
    104
} else {
    108
};

/// Rejects an ownership socket path that `bind(2)` would refuse, naming the
/// root so the operator knows what to shorten.
pub(super) fn check_socket_path(root: &Path, socket: &Path) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let length = socket.as_os_str().as_bytes().len();
    if length < SOCKET_PATH_LIMIT {
        return Ok(());
    }
    let root_length = root.as_os_str().as_bytes().len();
    Err(std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        format!(
            "ownership socket path {} is {length} bytes, but a Unix socket path must be shorter \
             than {SOCKET_PATH_LIMIT} bytes; the ownership root {} is {root_length} bytes and \
             the socket name adds {} more. Use a shorter ownership root",
            socket.display(),
            root.display(),
            length - root_length,
        ),
    ))
}

pub(super) fn socket_path(root: &Path, id: &str) -> Result<PathBuf, RegistryError> {
    if id.len() != 32 || !id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(super::OwnershipError::Fenced.into());
    }
    Ok(root.join(format!("{id}.sock")))
}

impl Drop for OwnerHost {
    fn drop(&mut self) {
        if self.release_on_drop.load(Ordering::SeqCst) {
            let _ = self.quiesce();
        }
        let _ = self.archive.stop();
        let _ = self.stop_owner_worker();
    }
}

/// Best-effort cleanup; uncertainty always preserves scratch.
fn clean_scratch(db: &Path, thread: &str, lease: &Lease, scratch: &ThreadScratch, reset: bool) {
    let result = Registry::open_existing(db).and_then(|mut registry| {
        registry.clean_scratch(thread, lease, || {
            if reset {
                scratch.reset_thread(thread);
            } else {
                scratch.release_thread(thread);
            }
        })
    });
    if let Err(error) = result {
        tracing::warn!(%thread, %error, "scratch cleanup skipped");
    }
}

/// Delete scratch directories of threads without a live owner, before this
/// host can claim any. Uncertain liveness keeps a directory, as for claims.
fn sweep_scratch(scratch: &ThreadScratch, root: &Path, owner_id: &str, registry: &mut Registry) {
    let result = registry.scratch_sweep(|owners| {
        let live = owners
            .into_iter()
            .filter(|owner| {
                owner.protects_scratch()
                    || (owner.state != OwnerState::Released
                        && ipc::owner_may_be_live(root, &owner.lease.owner_id))
            })
            .map(|owner| owner.thread_id)
            .collect();
        let other_process_may_be_live = std::fs::read_dir(root).map_or(true, |entries| {
            entries.filter_map(Result::ok).any(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .and_then(|name| name.strip_suffix(".sock"))
                    .is_some_and(|id| id != owner_id && ipc::owner_may_be_live(root, id))
            })
        });
        scratch.sweep(&live, other_process_may_be_live);
    });
    if let Err(error) = result {
        tracing::warn!(%error, "scratch sweep skipped: owner registry unreadable");
    }
}

fn emit(bus: &EventBus, owner: &ThreadOwner, action: OwnershipAction) {
    bus.emit(Event::new(OwnershipEvent {
        thread_id: owner.thread_id.clone(),
        owner_id: owner.lease.owner_id.clone(),
        generation: owner.lease.generation,
        action,
    }));
}

/// What the heartbeat worker needs besides its registry connection.
#[derive(Clone, Copy)]
struct Worker<'a> {
    db: &'a Path,
    id: &'a str,
    settings: &'a config::OwnershipConfig,
    bus: &'a EventBus,
    release_deferred: &'a AtomicBool,
    scratch: &'a ThreadScratch,
}

fn serve(
    mut registry: Registry,
    listener: UnixListener,
    stop: mpsc::Receiver<()>,
    worker: Worker<'_>,
) {
    let Worker {
        db,
        id,
        settings,
        bus,
        scratch,
        ..
    } = worker;
    let mut heartbeat = std::time::Instant::now();
    // Generations this host held at its last scan. One that is gone from a
    // later scan was taken over (after this process stopped answering) or
    // released; either way its runs lost authority.
    let mut held = std::collections::BTreeMap::<String, Lease>::new();
    loop {
        match listener.accept() {
            Ok((mut stream, _)) => {
                let _ = ipc::serve_connection_releasing(
                    &mut stream,
                    &mut registry,
                    now_ms(),
                    bus,
                    &|thread, previous, current| {
                        if previous.owner_id == id {
                            authority::record_fenced(db, thread, previous);
                            clean_scratch(db, thread, &current.lease, scratch, false);
                        }
                    },
                );
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(_) => break,
        }
        if heartbeat.elapsed() >= Duration::from_millis(settings.heartbeat_ms.get()) {
            heartbeat_tick(&mut registry, &mut held, worker);
            heartbeat = std::time::Instant::now();
        }
        match stop.recv_timeout(Duration::from_millis(10)) {
            Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
}

/// One heartbeat tick: detect generations this host lost since the last tick,
/// then renew or release every held thread that needs it in one transaction.
/// Threads whose leases have time left are not written at all.
fn heartbeat_tick(
    registry: &mut Registry,
    held: &mut std::collections::BTreeMap<String, Lease>,
    worker: Worker<'_>,
) {
    let Worker {
        db,
        id,
        settings,
        bus,
        release_deferred,
        scratch,
    } = worker;
    let owners = match registry.list() {
        Ok(owners) => owners,
        Err(error) => {
            tracing::warn!(owner_id = %id, %error, "owner heartbeat registry scan failed");
            return;
        }
    };
    let owned: Vec<_> = owners
        .into_iter()
        .filter(|owner| owner.lease.owner_id == id && owner.state != OwnerState::Released)
        .collect();
    for (thread, lease) in held.iter() {
        if !owned
            .iter()
            .any(|owner| owner.thread_id == *thread && owner.lease.generation == lease.generation)
        {
            authority::record_fenced(db, thread, lease);
        }
    }
    // Contention keeps every generation: only an observed fence revokes one.
    *held = owned
        .iter()
        .map(|owner| (owner.thread_id.clone(), owner.lease.clone()))
        .collect();
    let now = now_ms();
    let heartbeat_ms = settings.heartbeat_ms.get();
    let release = !release_deferred.load(Ordering::SeqCst);
    if owned
        .iter()
        .all(|owner| owner.heartbeat_tick(now, heartbeat_ms, release) == super::HeartbeatTick::Keep)
    {
        return;
    }
    let outcomes = match registry.heartbeat_owned(id, now, heartbeat_ms, release) {
        Ok(outcomes) => outcomes,
        Err(error) => {
            tracing::warn!(owner_id = %id, %error, "owner heartbeat update failed");
            return;
        }
    };
    for super::registry::HeartbeatOutcome { previous, result } in outcomes {
        match result {
            Ok((super::registry::HeartbeatChange::Kept, _)) => {}
            Ok((super::registry::HeartbeatChange::Renewed, owner)) => {
                #[cfg(feature = "recovery-test-support")]
                super::heartbeat_observation::record(bus, &owner);
                emit(bus, &owner, OwnershipAction::Heartbeat);
            }
            Ok((super::registry::HeartbeatChange::Released, owner)) => {
                authority::record_fenced(db, &previous.thread_id, &previous.lease);
                held.remove(&previous.thread_id);
                clean_scratch(db, &owner.thread_id, &owner.lease, scratch, false);
                emit(bus, &owner, OwnershipAction::Released);
            }
            Err(error) => {
                if matches!(error, super::OwnershipError::Fenced) {
                    authority::record_fenced(db, &previous.thread_id, &previous.lease);
                    held.remove(&previous.thread_id);
                }
                tracing::warn!(
                    thread_id = %previous.thread_id,
                    owner_id = %previous.lease.owner_id,
                    generation = previous.lease.generation,
                    %error,
                    "owner heartbeat update failed"
                );
            }
        }
    }
}
