//! Target-only PTY description. No raw Command, slave or PGID ownership entry.
use super::{OwnedCommand, OwnedPtyChild, OwnedPtyMaster};
use crate::ledger::{ChildKind, SpawnLedger};
use std::{ffi::OsStr, io, path::Path};

#[derive(Debug)]
pub struct OwnedPtyCommand(OwnedCommand);
impl OwnedPtyCommand {
    pub fn new(program: impl AsRef<OsStr>) -> Self {
        Self(OwnedCommand::new(program))
    }
    pub(crate) fn from_target(target: OwnedCommand) -> Self {
        Self(target)
    }
    pub(super) fn target(&self) -> &OwnedCommand {
        &self.0
    }
    pub fn arg(&mut self, arg: impl AsRef<OsStr>) -> &mut Self {
        self.0.arg(arg);
        self
    }
    pub fn args<I, S>(&mut self, args: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.0.args(args);
        self
    }
    pub fn env(&mut self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> &mut Self {
        self.0.env(key, value);
        self
    }
    pub fn env_clear(&mut self) -> &mut Self {
        self.0.env_clear();
        self
    }
    pub fn env_remove(&mut self, key: impl AsRef<OsStr>) -> &mut Self {
        self.0.env_remove(key);
        self
    }
    pub fn current_dir(&mut self, dir: impl AsRef<Path>) -> &mut Self {
        self.0.current_dir(dir);
        self
    }
    #[cfg(target_os = "linux")]
    pub fn guard_runtime(&mut self, runtime: &process_guard::GuardRuntime) -> &mut Self {
        self.0.guard_runtime(runtime);
        self
    }
}
impl OwnedPtyChild {
    /// One blocking ownership operation: admission, openpty, authenticated exec,
    /// and registration. A cancelled await still drops its completed RAII owner.
    /// No async worker performs the guard handshake or blocking retirement.
    pub async fn spawn_blocking(
        ledger: Option<SpawnLedger>,
        kind: ChildKind,
        label: Option<String>,
        command: OwnedPtyCommand,
        size: portable_pty::PtySize,
    ) -> io::Result<(Self, OwnedPtyMaster)> {
        super::offload(move || {
            let admission =
                SpawnLedger::admit_optional(ledger.as_ref()).map_err(io::Error::from)?;
            admission.spawn_owned_pty(kind, label.as_deref(), &command, size)
        })
        .await
    }
}
