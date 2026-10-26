//! Synchronous process ownership for incremental stdio protocols.

use std::{
    io,
    process::{Child, ChildStderr, ChildStdin, ChildStdout, ExitStatus, Stdio},
};

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
}

impl StdioSession {
    /// Starts only the command produced by the supplied sandbox's policy path.
    ///
    /// # Errors
    /// Returns the original wrapping error or the OS process-spawn error.
    pub fn spawn(sandbox: &dyn Sandbox, spec: CommandSpec) -> Result<Self, SessionError> {
        let wrapped = sandbox.wrap(spec)?;
        let mut command = wrapped.to_command();
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let child = command.spawn()?;
        let group = ProcessGroup::new(Some(child.id()));
        Ok(Self { child, group })
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
                self.group.kill_and_reap(|| child.try_wait())
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
        let _ = self.group.kill_and_reap(|| {
            let _ = child.kill();
            loop {
                match child.wait() {
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    result => return result.map(Some),
                }
            }
        });
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
