//! Non-PTY ownership API (D-3a2 B2a; callers migrate in B2b parts). Remaining raw owners stay until B2b-iii.
//! Linux uses an authenticated process guard, regardless of ledger presence.
mod command;
use crate::{
    CommandSpec, Sandbox, SessionError,
    ledger::{Admission, ChildKind, SpawnLedger},
};
pub use command::OwnedCommand;
#[cfg(target_os = "linux")]
pub use process_guard::GuardRuntime;
use std::{io, process::Stdio};
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::{OwnedChild, OwnedGroup, OwnedSession};

#[cfg(not(target_os = "linux"))]
mod unix;
#[cfg(not(target_os = "linux"))]
pub use unix::{OwnedChild, OwnedGroup, OwnedSession};

impl OwnedSession {
    /// Starts the command the supplied sandbox's policy path produces, with all
    /// three standard streams piped, as a [`ChildKind::SandboxSession`] under the
    /// ledger's admission (when there is one). Wrapping never falls back to
    /// direct execution. Needs no Tokio runtime; call it from a blocking thread.
    ///
    /// # Errors
    /// The wrapping error, an I/O error when the ledger's spawn fence is closed
    /// (nothing started), or the launch error.
    pub fn spawn_sandboxed(
        ledger: Option<&SpawnLedger>,
        sandbox: &dyn Sandbox,
        spec: CommandSpec,
    ) -> Result<Self, SessionError> {
        let admission = SpawnLedger::admit_optional(ledger).map_err(io::Error::from)?;
        Self::spawn_sandboxed_in(&admission, sandbox, spec)
    }

    /// [`spawn_sandboxed`](Self::spawn_sandboxed) inside an existing admission.
    ///
    /// # Errors
    /// The wrapping error or the launch error.
    pub fn spawn_sandboxed_in(
        admission: &Admission<'_>,
        sandbox: &dyn Sandbox,
        spec: CommandSpec,
    ) -> Result<Self, SessionError> {
        let mut command = sandbox.wrap(spec)?.to_owned_command();
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        Ok(Self::spawn_in(
            admission,
            ChildKind::SandboxSession,
            None,
            &mut command,
        )?)
    }

    /// [`spawn_in`](Self::spawn_in) for a site whose ledger is optional: admits
    /// the launch itself (a plain synchronous call). Without a ledger the child
    /// is still guard-owned, just unregistered.
    ///
    /// # Errors
    /// An I/O error when the ledger's spawn fence is closed (nothing started),
    /// otherwise the launch error.
    pub fn spawn_owned(
        ledger: Option<&SpawnLedger>,
        kind: ChildKind,
        label: Option<&str>,
        command: &mut OwnedCommand,
    ) -> io::Result<Self> {
        let admission = SpawnLedger::admit_optional(ledger).map_err(io::Error::from)?;
        Self::spawn_in(&admission, kind, label, command)
    }
}

/// Bind the immutable bundle beside this running application. Call before
/// config, logging or async runtime creation; an inherited env cannot override it.
pub fn bind_installed(role: &str) -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    process_guard::GuardRuntime::bind_installed(role).map(drop)?;
    #[cfg(not(target_os = "linux"))]
    let _ = role;
    Ok(())
}
