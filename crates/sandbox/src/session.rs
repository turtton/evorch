//! Synchronous process ownership for incremental stdio protocols.

use std::{
    io,
    process::{Child, ChildStderr, ChildStdin, ChildStdout, ExitStatus, Stdio},
};

use crate::ledger::{Admission, ChildEntry, ChildKind, SpawnLedger};
use crate::process::{self, ProcessGroup};
use crate::{CommandSpec, Sandbox, SandboxError};

/// Failure to wrap or start a stdio session; wrapping never falls back to direct execution.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error(transparent)]
    Sandbox(#[from] SandboxError),
    #[error("stdio session I/O: {0}")]
    Io(#[from] io::Error),
}

/// Owns a wrapped child until it is killed and reaped, even after its pipes are taken.
///
/// This synchronous seam matches `Sandbox::wrap` and does not require a Tokio runtime.
/// `take_*` transfers standard `Read`/`Write` handles to the consumer. Drain stderr
/// concurrently (for example with `io::copy` into a sink); leaving a full stderr pipe
/// unread blocks the child. Read stdout concurrently with large writes, use bounded
/// request buffers, and do not read the entire stream to EOF for a response.
/// Async consumers must move blocking I/O off executor threads or adapt the handles.
///
/// The child starts under the launch policy of [`crate::process`]: its own session
/// (and process group), without evorch's terminal or descriptors. Its group is
/// killed whenever the child is reaped, by [`try_wait`](Self::try_wait) or by
/// drop, and never after: the descendants that stayed in it stop with the child,
/// and a reaped id is not signalled. Drop reaps the child synchronously,
/// independent of runtime shutdown. Bubblewrap's existing `--die-with-parent`
/// and filesystem/network policy are preserved by `wrap`.
#[derive(Debug)]
pub struct StdioSession {
    child: Child,
    group: ProcessGroup,
    entry: Option<ChildEntry>,
}

impl StdioSession {
    /// Starts only the command produced by the supplied sandbox's policy path.
    ///
    /// # Errors
    /// Returns the original wrapping error or the OS process-spawn error.
    pub fn spawn(sandbox: &dyn Sandbox, spec: CommandSpec) -> Result<Self, SessionError> {
        Self::spawn_in(&Admission::unmanaged(), sandbox, spec)
    }

    /// [`spawn`](Self::spawn) inside a ledger admission: the child is
    /// registered as a [`ChildKind::SandboxSession`] and the final spawn fence
    /// waits for this launch.
    ///
    /// # Errors
    /// Returns the original wrapping error or the OS process-spawn error.
    pub fn spawn_in(
        admission: &Admission<'_>,
        sandbox: &dyn Sandbox,
        spec: CommandSpec,
    ) -> Result<Self, SessionError> {
        let wrapped = sandbox.wrap(spec)?;
        let mut command = wrapped.to_command();
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let child = command.spawn()?;
        let group = ProcessGroup::new(Some(child.id()));
        let entry = admission.register_group(ChildKind::SandboxSession, None, &group);
        Ok(Self {
            child,
            group,
            entry,
        })
    }

    /// Starts `command` (stdio as the caller configured it) as a session of
    /// `kind`, under the launch policy, registered in the admission's ledger.
    /// For a short-lived program evorch runs itself rather than through a
    /// sandbox, such as a read-only `git`. Dropping the session kills the group
    /// and reaps the leader.
    ///
    /// # Errors
    /// The OS process-spawn error.
    pub fn spawn_command_in(
        admission: &Admission<'_>,
        kind: ChildKind,
        label: Option<&str>,
        mut command: std::process::Command,
    ) -> io::Result<Self> {
        process::apply_launch_policy(&mut command);
        let child = command.spawn()?;
        let group = ProcessGroup::new(Some(child.id()));
        let entry = admission.register_group(kind, label, &group);
        Ok(Self {
            child,
            group,
            entry,
        })
    }

    /// [`spawn_command_in`](Self::spawn_command_in) for a site whose ledger is
    /// optional: admits the launch itself (a plain synchronous call).
    ///
    /// # Errors
    /// An I/O error when the ledger's spawn fence is closed (nothing started),
    /// otherwise the OS process-spawn error.
    pub fn spawn_command_owned(
        ledger: Option<&SpawnLedger>,
        kind: ChildKind,
        label: Option<&str>,
        command: std::process::Command,
    ) -> io::Result<Self> {
        let admission = SpawnLedger::admit_optional(ledger).map_err(io::Error::from)?;
        Self::spawn_command_in(&admission, kind, label, command)
    }

    /// [`spawn_in`](Self::spawn_in) for a site whose ledger is optional: admits
    /// the launch itself, so it is a plain synchronous call (blocking thread).
    /// Without a ledger it behaves as [`spawn`](Self::spawn).
    ///
    /// # Errors
    /// An I/O error when the ledger's spawn fence is closed (nothing started),
    /// otherwise the error of [`spawn_in`](Self::spawn_in).
    pub fn spawn_owned(
        ledger: Option<&SpawnLedger>,
        sandbox: &dyn Sandbox,
        spec: CommandSpec,
    ) -> Result<Self, SessionError> {
        let admission = SpawnLedger::admit_optional(ledger).map_err(io::Error::from)?;
        Self::spawn_in(&admission, sandbox, spec)
    }

    /// Writes `input` (when given) to the child's stdin, collects its stdout and
    /// stderr, and waits for it, like [`std::process::Child::wait_with_output`].
    /// The exited leader's group is killed before it is reaped, so a helper that
    /// kept the pipes open cannot hold the reads. Needs `command`'s stdio to be
    /// piped, as [`spawn_command_in`](Self::spawn_command_in) leaves it.
    ///
    /// # Errors
    /// A failed write (other than a closed pipe), read or wait.
    pub fn output(mut self, input: Option<&[u8]>) -> io::Result<std::process::Output> {
        use std::io::{Read, Write};

        let read_all = |pipe: Option<Box<dyn Read + Send>>| {
            move || {
                let mut bytes = Vec::new();
                pipe.map_or(Ok(()), |mut pipe| pipe.read_to_end(&mut bytes).map(drop))
                    .map(|()| bytes)
            }
        };
        let stdin = self.take_stdin();
        let stdout = self.take_stdout().map(|pipe| Box::new(pipe) as _);
        let stderr = self.take_stderr().map(|pipe| Box::new(pipe) as _);
        std::thread::scope(|scope| {
            let stdout = scope.spawn(read_all(stdout));
            let stderr = scope.spawn(read_all(stderr));
            let written = match (stdin, input) {
                (Some(mut stdin), Some(input)) => match stdin.write_all(input) {
                    Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(()),
                    result => result,
                },
                _ => Ok(()),
            };
            let status = self.wait_for_status();
            let join = |reader: std::thread::ScopedJoinHandle<'_, io::Result<Vec<u8>>>| {
                reader
                    .join()
                    .unwrap_or_else(|_| Err(io::Error::other("an output reader panicked")))
            };
            let (stdout, stderr) = (join(stdout), join(stderr));
            written?;
            Ok(std::process::Output {
                status: status?,
                stdout: stdout?,
                stderr: stderr?,
            })
        })
    }

    pub(crate) fn wait_for_status(&mut self) -> io::Result<ExitStatus> {
        let Some(leader) = self.group.leader() else {
            return self.child.wait();
        };
        process::wait_for_exit(leader)?;
        let child = &mut self.child;
        let status = self
            .group
            .kill_and_reap(|| child.try_wait())?
            .ok_or_else(|| io::Error::other("the exited child was not reapable"))?;
        self.note_reaped();
        Ok(status)
    }

    #[cfg(not(target_os = "linux"))]
    pub(crate) fn owned_group(&self) -> &ProcessGroup {
        &self.group
    }

    pub fn id(&self) -> u32 {
        self.child.id()
    }

    pub const fn take_stdin(&mut self) -> Option<ChildStdin> {
        self.child.stdin.take()
    }

    pub const fn take_stdout(&mut self) -> Option<ChildStdout> {
        self.child.stdout.take()
    }

    pub const fn take_stderr(&mut self) -> Option<ChildStderr> {
        self.child.stderr.take()
    }

    fn note_reaped(&mut self) {
        if let Some(entry) = self.entry.take() {
            entry.reaped();
        }
    }

    /// Polls and reaps an exited child without blocking on a running child.
    ///
    /// The exit is observed first without reaping, so what is left of the
    /// group is killed while the exited child still reserves its id.
    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        let Some(leader) = self.group.leader() else {
            return self.child.try_wait();
        };
        match process::has_exited(leader) {
            Ok(false) => Ok(None),
            Ok(true) => {
                let child = &mut self.child;
                let reaped = self.group.kill_and_reap(|| child.try_wait())?;
                if reaped.is_some() {
                    self.note_reaped();
                }
                Ok(reaped)
            }
            Err(error) => {
                self.group.release();
                Err(error)
            }
        }
    }
}

impl Drop for StdioSession {
    fn drop(&mut self) {
        // Drop cannot return errors. Still wait if kill reports an already-exited
        // child, and retry interrupted waits so a signal cannot leave a zombie.
        // The group goes first, while the unreaped child still reserves its id;
        // after try_wait reaped the child, nothing is signalled.
        let child = &mut self.child;
        let reaped = self.group.kill_and_reap(|| {
            let _ = child.kill();
            loop {
                match child.wait() {
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    result => return result.map(Some),
                }
            }
        });
        if matches!(reaped, Ok(Some(_))) {
            self.note_reaped();
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::io::{BufRead, BufReader};
    use std::time::{Duration, Instant};

    use super::*;
    use crate::DirectSandbox;

    fn stopped(pid: &str) -> bool {
        std::fs::read_to_string(format!("/proc/{pid}/stat")).map_or(true, |stat| {
            stat.rsplit_once(')')
                .is_some_and(|(_, rest)| rest.trim_start().starts_with('Z'))
        })
    }

    #[test]
    fn output_collects_the_streams_and_stops_a_helper_that_holds_the_pipes() {
        // Given: a command that echoes its input, then leaves a helper holding
        // its stdout open as it exits.
        let ledger = SpawnLedger::new();
        let mut command = std::process::Command::new("sh");
        command
            .args(["-c", "cat; echo err >&2; sh -c 'kill -STOP $$' &"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let session = StdioSession::spawn_command_owned(
            Some(&ledger),
            ChildKind::Other,
            Some("output-test"),
            command,
        )
        .unwrap();
        assert_eq!(ledger.children().len(), 1);

        // When: its output is collected.
        let output = session.output(Some(b"hello")).unwrap();

        // Then: the reads ended, which they can only once the helper holding
        // the pipes is gone, and the ledger saw the leader reaped.
        assert!(output.status.success());
        assert_eq!(output.stdout, b"hello");
        assert_eq!(output.stderr, b"err\n");
        let stats = ledger.child_stats();
        assert_eq!(
            (stats.registered, stats.reaped, stats.unconfirmed),
            (1, 1, 0)
        );
    }

    #[test]
    fn a_command_session_under_a_closed_fence_does_not_start() {
        let ledger = SpawnLedger::new();
        ledger.close_fence();

        let error = StdioSession::spawn_command_owned(
            Some(&ledger),
            ChildKind::Other,
            None,
            std::process::Command::new("true"),
        )
        .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert_eq!(ledger.child_stats().registered, 0);
    }

    #[test]
    fn try_wait_cleans_up_the_group_when_it_reaps_and_retires_it() {
        // Given: a server that leaves a helper behind and exits.
        let mut session = StdioSession::spawn(
            &DirectSandbox::new_unchecked(),
            CommandSpec {
                program: "sh".into(),
                args: vec!["-c".into(), "sleep 60 & echo $!".into()],
                cwd: None,
                extra_env: Vec::new(),
            },
        )
        .unwrap();
        let mut line = String::new();
        BufReader::new(session.take_stdout().unwrap())
            .read_line(&mut line)
            .unwrap();
        let helper = line.trim().to_owned();

        // When: try_wait observes the exit.
        let deadline = Instant::now() + Duration::from_secs(10);
        let status = loop {
            if let Some(status) = session.try_wait().unwrap() {
                break status;
            }
            assert!(Instant::now() < deadline, "the server did not exit");
            std::thread::sleep(Duration::from_millis(10));
        };

        // Then: the helper stopped with the reap, and the reaped group id is
        // retired before drop, so drop signals nothing.
        assert!(status.success());
        let deadline = Instant::now() + Duration::from_secs(10);
        while !stopped(&helper) {
            assert!(Instant::now() < deadline, "helper {helper} still runs");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(session.group.leader(), None);
        assert!(session.try_wait().unwrap().is_some());
        drop(session);
    }
}
