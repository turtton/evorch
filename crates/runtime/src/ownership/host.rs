use std::os::unix::{fs::PermissionsExt, net::UnixListener};
use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use event_bus::{Event, EventBus, OwnershipAction, OwnershipEvent};

use super::{
    Lease, OwnerPermit, OwnerState, Registry, RegistryError, ThreadOwner, ipc, permit::now_ms,
};

pub struct OwnerHost {
    root: PathBuf,
    owner_id: String,
    settings: config::OwnershipConfig,
    bus: Arc<EventBus>,
    stop: mpsc::Sender<()>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl OwnerHost {
    pub fn open(
        root: &Path,
        settings: config::OwnershipConfig,
        bus: Arc<EventBus>,
    ) -> Result<Self, RegistryError> {
        std::fs::create_dir_all(root)?;
        std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700))?;
        let mut random = [0u8; 16];
        getrandom::fill(&mut random).map_err(std::io::Error::other)?;
        let owner_id = random
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let socket = root.join(format!("{owner_id}.sock"));
        let listener = UnixListener::bind(&socket)?;
        listener.set_nonblocking(true)?;
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
        let registry = Registry::open(&root.join("owners.db"))?;
        let (stop, receiver) = mpsc::channel();
        let id = owner_id.clone();
        let worker_settings = settings.clone();
        let worker_bus = Arc::clone(&bus);
        let worker = std::thread::spawn(move || {
            serve(
                registry,
                listener,
                receiver,
                &id,
                &worker_settings,
                &worker_bus,
            )
        });
        Ok(Self {
            root: root.into(),
            owner_id,
            settings,
            bus,
            stop,
            worker: Some(worker),
        })
    }

    pub fn attach(&self, thread_id: &str) -> Result<ThreadOwner, RegistryError> {
        Registry::open(&self.root.join("owners.db"))?.attach(thread_id)
    }

    pub fn start(&self, thread_id: &str) -> Result<OwnerPermit, RegistryError> {
        let owner = ThreadOwner::new(
            thread_id.into(),
            Lease {
                owner_id: self.owner_id.clone(),
                generation: 1,
                expires_at: now_ms().saturating_add(self.settings.lease_ms.get()),
            },
        );
        Registry::open(&self.root.join("owners.db"))?.start(&owner)?;
        emit(&self.bus, &owner, OwnershipAction::Claimed);
        Ok(self.permit(&owner))
    }

    pub fn claim(&self, expected: &ThreadOwner) -> Result<OwnerPermit, RegistryError> {
        let mut registry = Registry::open(&self.root.join("owners.db"))?;
        let socket = self.socket(&expected.lease.owner_id)?;
        let owner = ipc::claim(
            &mut registry,
            ipc::ClaimRequest {
                thread_id: &expected.thread_id,
                expected: &expected.lease,
                previous_socket: &socket,
                owner_id: &self.owner_id,
                now_ms: now_ms(),
                grace_ms: self.settings.grace_ms.get(),
            },
        )?;
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

    pub fn quiesce(&self) -> Result<bool, RegistryError> {
        let mut registry = Registry::open(&self.root.join("owners.db"))?;
        let mut active = false;
        for owner in registry.list()?.into_iter().filter(|owner| {
            owner.lease.owner_id == self.owner_id && owner.state != OwnerState::Released
        }) {
            let changed = registry.update(&owner.thread_id, |state| state.quiesce(&owner.lease))?;
            active |= changed.active_turn;
            emit(&self.bus, &changed, OwnershipAction::Quiescing);
            if !changed.active_turn {
                let released =
                    registry.update(&owner.thread_id, |state| state.release(&owner.lease))?;
                emit(&self.bus, &released, OwnershipAction::Released);
            }
        }
        Ok(active)
    }

    pub fn has_active_turns(&self) -> Result<bool, RegistryError> {
        Ok(Registry::open(&self.root.join("owners.db"))?
            .list()?
            .iter()
            .any(|owner| owner.lease.owner_id == self.owner_id && owner.active_turn))
    }

    fn permit(&self, owner: &ThreadOwner) -> OwnerPermit {
        OwnerPermit {
            registry_path: self.root.join("owners.db"),
            thread_id: owner.thread_id.clone(),
            lease: owner.lease.clone(),
            run_id: None,
        }
    }

    fn socket(&self, id: &str) -> Result<PathBuf, RegistryError> {
        if id.len() != 32 || !id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(super::OwnershipError::Fenced.into());
        }
        Ok(self.root.join(format!("{id}.sock")))
    }
}

impl Drop for OwnerHost {
    fn drop(&mut self) {
        let _ = self.quiesce();
        let _ = self.stop.send(());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        let _ = std::fs::remove_file(self.root.join(format!("{}.sock", self.owner_id)));
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

fn serve(
    mut registry: Registry,
    listener: UnixListener,
    stop: mpsc::Receiver<()>,
    id: &str,
    settings: &config::OwnershipConfig,
    bus: &EventBus,
) {
    let mut heartbeat = std::time::Instant::now();
    loop {
        match listener.accept() {
            Ok((mut stream, _)) => {
                let _ = ipc::serve_connection_with_bus(&mut stream, &mut registry, now_ms(), bus);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(_) => break,
        }
        if heartbeat.elapsed() >= Duration::from_millis(settings.heartbeat_ms.get()) {
            if let Ok(owners) = registry.list() {
                for owner in owners.into_iter().filter(|owner| {
                    owner.lease.owner_id == id && owner.state != OwnerState::Released
                }) {
                    let result = registry.update(&owner.thread_id, |state| {
                        state.validate(&owner.lease)?;
                        if state.state == OwnerState::Quiescing && !state.active_turn {
                            state.release(&owner.lease)?;
                        } else {
                            state.lease.expires_at =
                                now_ms().saturating_add(settings.lease_ms.get());
                        }
                        Ok(())
                    });
                    if let Ok(changed) = result {
                        emit(
                            bus,
                            &changed,
                            if changed.state == OwnerState::Released {
                                OwnershipAction::Released
                            } else {
                                OwnershipAction::Heartbeat
                            },
                        );
                    }
                }
            }
            heartbeat = std::time::Instant::now();
        }
        match stop.recv_timeout(Duration::from_millis(10)) {
            Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
}
