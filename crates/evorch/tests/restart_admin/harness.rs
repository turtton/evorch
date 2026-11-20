#![allow(dead_code)]

use super::fake_core::*;
use core_launcher::handoff::diagnostics::{connect::ConnectTarget, reconnect::ReconnectPolicy};
use evorch::cli::core_admin::{AdminCommand, AdminEnv, run};
use std::{
    future::Future,
    io::Write,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::Notify;
use workbench_client::ClientConfig;
use workbench_protocol::{
    handshake::{BuildInfo, Capability, LocalSurface},
    ids::ClientId,
};

/// What the command printed. Tests wait for a line, never for time.
#[derive(Default)]
pub struct Capture {
    text: Mutex<String>,
    changed: Notify,
}
impl Capture {
    pub fn new() -> Arc<Self> {
        Arc::default()
    }
    pub fn text(&self) -> String {
        self.text.lock().unwrap().clone()
    }
    pub async fn wait_contains(&self, needle: &str) {
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.text().contains(needle) {
                return;
            }
            notified.await;
        }
    }
}
pub struct Writer(pub Arc<Capture>);
impl Write for Writer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0
            .text
            .lock()
            .unwrap()
            .push_str(&String::from_utf8_lossy(bytes));
        self.0.changed.notify_waiters();
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub struct Env {
    pub config: tempfile::TempDir,
    _sockets: tempfile::TempDir,
    pub socket: PathBuf,
    pub target: ConnectTarget,
}

pub fn env() -> Env {
    let config = tempfile::tempdir().unwrap();
    let sockets = tempfile::Builder::new()
        .prefix("cn")
        .tempdir_in("/tmp")
        .unwrap();
    std::fs::set_permissions(
        sockets.path(),
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .unwrap();
    let socket = std::fs::canonicalize(sockets.path()).unwrap().join("s");
    let identity = core_instance::ConfigIdentity::new(config.path()).unwrap();
    Env {
        target: ConnectTarget::with_socket(identity, socket.clone()),
        config,
        _sockets: sockets,
        socket,
    }
}

pub fn capabilities(safe_restart: bool) -> Vec<Capability> {
    let mut all = Capability::IMPLEMENTED.to_vec();
    if safe_restart && !all.contains(&Capability::SafeRestart) {
        all.push(Capability::SafeRestart);
    }
    all
}

pub fn core(env: &Env, epoch: u64) -> FakeCore {
    FakeCore::start(&env.socket, epoch, capabilities(true))
}

pub struct Terminal {
    pub is_terminal: bool,
    pub answer: bool,
    pub asked: Arc<AtomicUsize>,
}
impl Terminal {
    pub fn none() -> Self {
        Self {
            is_terminal: false,
            answer: false,
            asked: Arc::default(),
        }
    }
    pub fn answering(answer: bool) -> Self {
        Self {
            is_terminal: true,
            answer,
            asked: Arc::default(),
        }
    }
}

pub fn request(yes: bool, no_wait: bool) -> AdminCommand {
    AdminCommand::Request {
        target: workbench_client::restart::intent::TargetSelector::Current,
        yes,
        no_wait,
    }
}

pub struct Run {
    pub out: Arc<Capture>,
    pub err: Arc<Capture>,
}
impl Run {
    pub fn new() -> Self {
        Self {
            out: Capture::new(),
            err: Capture::new(),
        }
    }

    pub async fn drive(
        &self,
        env: &Env,
        command: AdminCommand,
        terminal: &Terminal,
        interrupt: impl Future<Output = ()>,
    ) -> u8 {
        let mut client = ClientConfig::new(
            ClientId::new("cli-test").unwrap(),
            BuildInfo {
                name: "evorch-cli".into(),
                version: "0".into(),
                revision: None,
            },
        );
        client.local_surface = Some(LocalSurface::Cli);
        let mut out = Writer(self.out.clone());
        let mut err = Writer(self.err.clone());
        let asked = terminal.asked.clone();
        let answer = terminal.answer;
        let mut approve = move |_: &str| {
            asked.fetch_add(1, Ordering::SeqCst);
            answer
        };
        let interrupt = std::pin::pin!(interrupt);
        run(
            command,
            AdminEnv {
                target: env.target.clone(),
                client,
                policy: ReconnectPolicy {
                    poll: Duration::from_millis(5),
                    attempt_limit: Duration::from_secs(5),
                },
                terminal: terminal.is_terminal,
                out: &mut out,
                err: &mut err,
                approve: &mut approve,
                interrupt,
            },
        )
        .await
    }
}

/// The operation id of the one restart command a core was given.
pub fn the_operation(core: &FakeCore) -> String {
    let commands = core.commands();
    assert_eq!(commands.len(), 1, "exactly one command: {commands:?}");
    match &commands[0] {
        workbench_protocol::commands::Command::Lifecycle(c) => c.operation_id().as_str().to_owned(),
        other => panic!("not a lifecycle command: {other:?}"),
    }
}

/// No terminal and no flag: nothing can confirm.
pub static NONE: std::sync::LazyLock<Terminal> = std::sync::LazyLock::new(Terminal::none);

/// Run the CLI and the scenario that feeds it together. If the CLI ends
/// before the scenario does (it failed), the scenario is dropped and the
/// CLI's own output explains why, instead of the test waiting on a core
/// that nobody talks to.
pub async fn both<T>(
    cli: impl Future<Output = u8>,
    scenario: impl Future<Output = T>,
) -> (u8, Option<T>) {
    tokio::pin!(cli);
    tokio::pin!(scenario);
    let mut driven = None;
    loop {
        tokio::select! {
            code = &mut cli => return (code, driven),
            value = &mut scenario, if driven.is_none() => driven = Some(value),
        }
    }
}
