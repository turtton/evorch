//! Local cold-start coordination. This is deliberately separate from the wire
//! client: GUI/CLI integration and shared restart budgets belong to later PRs.
use core_instance::{ConfigIdentity, InstanceError, LaunchGuard, validate_socket};
use std::{
    fs, io,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::Duration,
};
use workbench_client::{ClientConfig, ClientError, TransportError, UnixTransport, WorkbenchClient};
use workbench_protocol::{handshake::CoreState, ids::CoreIdentity};

#[derive(Debug, thiserror::Error)]
pub enum LaunchError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Instance(#[from] InstanceError),
    #[error("core readiness/handshake rejected: {0}")]
    Readiness(String),
    #[error("core exited before ready: {0}")]
    Exited(std::process::ExitStatus),
    #[error("core not ready before diagnostic deadline; no lock was stolen and no core was killed")]
    NotReady,
}

/// A sibling distribution binary or explicitly configured absolute executable.
/// No PATH or current-working-directory lookup.
#[derive(Debug, Clone)]
pub struct CoreBinary(PathBuf);
impl CoreBinary {
    pub fn sibling() -> io::Result<Self> {
        let current = std::env::current_exe()?;
        Self::explicit(&current.with_file_name("evorch-core"))
    }
    pub fn explicit(path: &Path) -> io::Result<Self> {
        if !path.is_absolute() || fs::symlink_metadata(path)?.file_type().is_symlink() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "core binary must be an absolute regular executable",
            ));
        }
        let path = fs::canonicalize(path)?;
        let m = fs::metadata(&path)?;
        let uid = rustix::process::geteuid().as_raw();
        if !m.is_file()
            || m.mode() & 0o111 == 0
            || m.mode() & 0o6022 != 0
            || (m.uid() != uid && m.uid() != 0)
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "unsafe core executable",
            ));
        }
        for parent in path.ancestors().skip(1) {
            let m = fs::metadata(parent)?;
            // Root-owned sticky /tmp is safe; a writable distribution directory is not.
            if (m.uid() != uid && m.uid() != 0)
                || (m.mode() & 0o022 != 0 && !(m.uid() == 0 && m.mode() & 0o1000 != 0))
            {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "unsafe core executable directory",
                ));
            }
        }
        Ok(Self(path))
    }
}

#[derive(Debug, Clone, Copy)]
pub enum LaunchStage {
    WaitingForLaunch,
    BeforeSpawn,
    Spawned(u32),
    LifetimeBusy,
    AlreadyRunning,
}

pub struct Launcher {
    config: ConfigIdentity,
    binary: CoreBinary,
    standard_socket: PathBuf,
    client: ClientConfig,
    diagnostic_after: Duration,
}
impl Launcher {
    pub fn new(
        config: ConfigIdentity,
        binary: CoreBinary,
        client: ClientConfig,
    ) -> io::Result<Self> {
        let socket = config.default_socket()?;
        Self::with_socket(config, binary, client, socket)
    }
    /// Explicit socket selection avoids mutating process environment in embedders.
    pub fn with_socket(
        config: ConfigIdentity,
        binary: CoreBinary,
        mut client: ClientConfig,
        standard_socket: PathBuf,
    ) -> io::Result<Self> {
        core_instance::validate_endpoint_path(&standard_socket)?;
        client.desired_core = Some(CoreIdentity::new(config.identity()).map_err(io::Error::other)?);
        Ok(Self {
            config,
            binary,
            standard_socket,
            client,
            diagnostic_after: Duration::from_secs(30),
        })
    }
    /// A production diagnostic deadline, not a reason to force an owner out.
    pub fn diagnostic_after(mut self, duration: Duration) -> Self {
        self.diagnostic_after = duration;
        self
    }
    pub async fn connect_or_start(&self) -> Result<WorkbenchClient, LaunchError> {
        self.connect_or_start_observed(|_| ()).await
    }
    /// Instrumentation points also allow process tests to stop at exact phases.
    pub async fn connect_or_start_observed(
        &self,
        observe: impl Fn(LaunchStage),
    ) -> Result<WorkbenchClient, LaunchError> {
        tokio::time::timeout(self.diagnostic_after, self.run(&observe))
            .await
            .map_err(|_| LaunchError::NotReady)?
    }
    async fn connect(&self) -> Result<Option<WorkbenchClient>, LaunchError> {
        let mut candidates = Vec::new();
        if let Some(record) = self.config.read_record()? {
            candidates.push((record.socket, Some(record.epoch)));
        }
        if !candidates.iter().any(|(p, _)| p == &self.standard_socket) {
            candidates.push((self.standard_socket.clone(), None));
        }
        for (socket, epoch) in candidates {
            match validate_socket(&socket) {
                Ok(()) => (),
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e.into()),
            }
            let transport = match UnixTransport::connect(&socket).await {
                Ok(t) => t,
                Err(TransportError::Disconnected) => continue,
                Err(e) => return Err(LaunchError::Readiness(e.to_string())),
            };
            let client = match WorkbenchClient::connect(transport, self.client.clone()).await {
                Ok(client) => client,
                // The owner can exit after accept, before replying to hello.
                // Reenter launch arbitration and reread discovery under it.
                Err(ClientError::Transport(TransportError::Disconnected)) => return Ok(None),
                Err(e) => return Err(LaunchError::Readiness(e.to_string())),
            };
            // Version, identity and required capabilities are checked by the wire
            // client. A saved epoch must also agree with the live hello.
            let hello = client.server_hello();
            if hello.instance_epoch.0 == 0 || hello.status.state != CoreState::Ready {
                return Err(LaunchError::Readiness(
                    "epoch or core state mismatch".into(),
                ));
            }
            if epoch.is_some_and(|e| e != hello.instance_epoch.0) {
                // The record may predate this connection. Never attach using a
                // mismatched hint; reread it after acquiring launch instead.
                return Ok(None);
            }
            return Ok(Some(client));
        }
        Ok(None)
    }
    async fn run(&self, observe: &impl Fn(LaunchStage)) -> Result<WorkbenchClient, LaunchError> {
        if let Some(client) = self.connect().await? {
            return Ok(client);
        }
        // flock/filesystem changes have no portable async notification source.
        // Polling is production coordination; tests synchronize via observe/IPC.
        let mut tick = tokio::time::interval(Duration::from_millis(20));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let launch = loop {
            match LaunchGuard::acquire(&self.config) {
                Ok(l) => break l,
                Err(InstanceError::AlreadyRunning) => observe(LaunchStage::WaitingForLaunch),
                Err(e) => return Err(e.into()),
            }
            if let Some(client) = self.connect().await? {
                return Ok(client);
            }
            tick.tick().await;
        };
        let mut child = None;
        let mut attempted_spawn = false;
        loop {
            if let Some(client) = self.connect().await? {
                drop(launch); // close only; child independently holds the same OFD
                return Ok(client);
            }
            if !attempted_spawn {
                if launch.lifetime_available()? {
                    observe(LaunchStage::BeforeSpawn);
                    // Revalidate immediately before spawn, including directory permissions.
                    let binary = CoreBinary::explicit(&self.binary.0)?;
                    let mut command = Command::new(binary.0);
                    command
                        .arg("--config-dir")
                        .arg(self.config.config_dir())
                        .stdin(Stdio::null())
                        .stdout(Stdio::null())
                        .stderr(Stdio::inherit());
                    let spawned = launch.spawn(command)?;
                    observe(LaunchStage::Spawned(spawned.id()));
                    child = Some(Reap(Some(spawned)));
                    attempted_spawn = true;
                } else {
                    // A disconnected owner may release lifetime later. Probe
                    // again under this same launch guard, never while unlocked.
                    observe(LaunchStage::LifetimeBusy);
                }
            }
            if let Some(c) = child.as_mut()
                && let Some(status) = c.0.as_mut().expect("owned child").try_wait()?
            {
                if status.code() != Some(2) {
                    return Err(LaunchError::Exited(status));
                }
                // Another owner won: reconnect, never blindly respawn. A shared
                // restart budget can wrap this single cold-start attempt later.
                child = None;
                observe(LaunchStage::AlreadyRunning);
            }
            tick.tick().await;
        }
    }
}
// Reap without tying the lifetime of a successfully launched core to its client.
struct Reap(Option<Child>);
impl Drop for Reap {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            if matches!(child.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
    }
}
