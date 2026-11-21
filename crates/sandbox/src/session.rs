//! Session errors, and the synchronous child backend of the non-Linux owner adapter.

use std::io;

use crate::SandboxError;

/// Failure to wrap or start a stdio session; wrapping never falls back to direct execution.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error(transparent)]
    Sandbox(#[from] SandboxError),
    #[error("stdio session I/O: {0}")]
    Io(#[from] io::Error),
}

#[cfg(not(target_os = "linux"))]
pub(crate) use raw::StdioSession;

#[cfg(not(target_os = "linux"))]
mod raw {
    use std::{
        io,
        process::{Child, ChildStderr, ChildStdin, ChildStdout, ExitStatus},
    };

    use crate::ledger::{Admission, ChildEntry, ChildKind};
    use crate::process::{self, ProcessGroup};

    /// The non-Linux backend behind the owner adapter (Linux owners are guard-backed and
    /// never reach it). Owns a child until it is killed and reaped, even after its pipes are taken.
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
    pub(crate) struct StdioSession {
        child: Child,
        group: ProcessGroup,
        entry: Option<ChildEntry>,
    }

    impl StdioSession {
        /// Starts `command` (stdio as the caller configured it) as a session of
        /// `kind`, under the launch policy, registered in the admission's ledger.
        /// For a short-lived program evorch runs itself rather than through a
        /// sandbox, such as a read-only `git`. Dropping the session kills the group
        /// and reaps the leader.
        ///
        /// # Errors
        /// The OS process-spawn error.
        pub(crate) fn spawn_command_in(
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

        /// Writes `input` (when given) to the child's stdin, collects its stdout and
        /// stderr, and waits for it, like [`std::process::Child::wait_with_output`].
        /// The exited leader's group is killed before it is reaped, so a helper that
        /// kept the pipes open cannot hold the reads. Needs `command`'s stdio to be
        /// piped, as [`spawn_command_in`](Self::spawn_command_in) leaves it.
        ///
        /// # Errors
        /// A failed write (other than a closed pipe), read or wait.
        pub(crate) fn output(mut self, input: Option<&[u8]>) -> io::Result<std::process::Output> {
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

        pub(crate) fn id(&self) -> u32 {
            self.child.id()
        }

        pub(crate) const fn take_stdin(&mut self) -> Option<ChildStdin> {
            self.child.stdin.take()
        }

        pub(crate) const fn take_stdout(&mut self) -> Option<ChildStdout> {
            self.child.stdout.take()
        }

        pub(crate) const fn take_stderr(&mut self) -> Option<ChildStderr> {
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
        pub(crate) fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
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
}
