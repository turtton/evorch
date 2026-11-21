//! Non-PTY ownership API (D-3a2 B2a; every non-PTY caller migrated by B2b-i..iii; the PTY backend is B3).
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

impl OwnedChild {
    /// Admission, launch and ledger registration for a caller on an async task.
    ///
    /// The guard handshake is synchronous (the launch waits for the helper's
    /// authenticated `ExecOk`), so the whole of it, including the spawn fence's
    /// admission, runs on a Tokio blocking thread; no async worker is held.
    /// Ownership never leaves the guard: the child is created and registered on
    /// that thread and only then handed back. A caller that drops this future
    /// mid-launch does not cancel it; the launch finishes and its child is
    /// dropped, which kills and reaps its group (the ledger entry is confirmed
    /// by the reaper). No deadline is added or extended.
    ///
    /// Must run inside a Tokio runtime.
    ///
    /// # Errors
    /// An I/O error when the ledger's spawn fence is closed (nothing started)
    /// or the launch failed, or when the blocking launch itself panicked.
    pub async fn spawn_blocking(
        ledger: Option<SpawnLedger>,
        kind: ChildKind,
        label: Option<String>,
        mut command: OwnedCommand,
    ) -> io::Result<Self> {
        offload(move || {
            let admission =
                SpawnLedger::admit_optional(ledger.as_ref()).map_err(io::Error::from)?;
            Self::spawn_in(&admission, kind, label.as_deref(), &mut command)
        })
        .await
    }
}

/// Runs `work` on a Tokio blocking thread (which carries the runtime context,
/// so async resources can be registered there) and awaits it.
async fn offload<T: Send + 'static>(
    work: impl FnOnce() -> io::Result<T> + Send + 'static,
) -> io::Result<T> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| io::Error::other(format!("blocking launch failed: {error}")))?
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

#[cfg(test)]
mod tests {
    use super::offload;

    #[test]
    fn offloaded_work_runs_off_the_async_thread_with_runtime_context() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let caller = std::thread::current().id();
        let (thread, has_runtime) = runtime
            .block_on(offload(|| {
                Ok((
                    std::thread::current().id(),
                    tokio::runtime::Handle::try_current().is_ok(),
                ))
            }))
            .unwrap();
        assert_ne!(thread, caller);
        assert!(has_runtime);
    }
}
