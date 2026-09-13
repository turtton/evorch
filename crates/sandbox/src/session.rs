//! Synchronous process ownership for incremental stdio protocols.

use std::{
    io,
    process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, ExitStatus, Stdio},
};

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
/// Drop sends a kill and waits synchronously, independent of runtime shutdown. It
/// manages the immediate child, not arbitrary descendants. Bubblewrap's existing
/// `--die-with-parent` and filesystem/network policy are preserved by `wrap`.
#[derive(Debug)]
pub struct StdioSession {
    child: Child,
}

impl StdioSession {
    /// Starts only the command produced by the supplied sandbox's policy path.
    ///
    /// # Errors
    /// Returns the original wrapping error or the OS process-spawn error.
    pub fn spawn(sandbox: &dyn Sandbox, spec: CommandSpec) -> Result<Self, SessionError> {
        let wrapped = sandbox.wrap(spec)?;
        let mut command = Command::new(wrapped.program);
        command
            .args(wrapped.args)
            .env_clear()
            .envs(wrapped.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(cwd) = wrapped.cwd {
            command.current_dir(cwd);
        }
        Ok(Self {
            child: command.spawn()?,
        })
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
    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }
}

impl Drop for StdioSession {
    fn drop(&mut self) {
        // Drop cannot return errors. Still wait if kill reports an already-exited
        // child, and retry interrupted waits so a signal cannot leave a zombie.
        let _ = self.child.kill();
        loop {
            match self.child.wait() {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Ok(_) | Err(_) => break,
            }
        }
    }
}
