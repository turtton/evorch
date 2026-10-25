//! How evorch starts child processes: detached from its terminal, without its
//! descriptors, and stoppable together with their descendants.
//!
//! evorch may run from a terminal, and it holds descriptors it never meant to
//! share: the GUI's browser engine keeps databases, GPU buffers and IPC sockets
//! open without `O_CLOEXEC`, and a bubblewrap sandbox passes them on to the
//! sandboxed command. [`apply_launch_policy`] starts a child in its own session
//! and lets the program it executes inherit only stdin, stdout and stderr.
//! [`ProcessGroup`] then stops the child with its descendants, and
//! [`GroupChild`] does so for Tokio children without ever signalling a group
//! id that a reaped leader has released.
//!
//! These are pre-exec hooks, so the standard library forks the parent instead
//! of using `posix_spawn`; a large parent pays for copying its page tables on
//! every launch. Pseudo-terminal launches (`portable-pty`) already start a new
//! session and close inherited descriptors, and do not need them.

use std::io;
use std::os::unix::process::CommandExt;
use std::process::{Command, ExitStatus};
use std::sync::{Arc, Mutex, PoisonError};

use rustix::process::{Pid, Signal, WaitId, WaitIdOptions};

/// Start the child in a new session, which is also a new process group.
///
/// A child left in evorch's session sits in a background process group of
/// evorch's controlling terminal: reading `/dev/tty` (a git credential or ssh
/// password prompt) stops it with SIGTTIN, and it never finishes. Without a
/// controlling terminal, opening `/dev/tty` fails and the child reports the
/// error instead. The session and process group ids both equal the child's
/// pid, so killing its [`ProcessGroup`] reaches the child and every descendant
/// that stays in its group.
///
/// Do not combine this with [`CommandExt::process_group`]: the standard library
/// moves the child into the requested group before it runs pre-exec hooks, a
/// process group leader cannot start a session, and the spawn fails with
/// `EPERM`. The new session already is the child's own group.
pub fn detach_from_terminal(command: &mut Command) {
    // SAFETY: between fork and exec the hook makes one async-signal-safe
    // system call and allocates nothing.
    unsafe {
        command.pre_exec(|| {
            rustix::process::setsid()?;
            Ok(())
        });
    }
}

/// Close every descriptor above stderr when the child executes its program.
///
/// The child keeps the standard streams configured on `command`; nothing else
/// evorch has open reaches the executed program. The descriptors are marked
/// close-on-exec rather than closed, which keeps the pipe the standard library
/// uses to report a failed exec (a missing program) working until the exec
/// succeeds.
///
/// Linux needs 5.11 or later (`close_range` with `CLOSE_RANGE_CLOEXEC`); on an
/// older kernel the spawn fails instead of leaking the descriptors. Other Unix
/// systems keep the standard inheritance.
pub fn close_inherited_fds_on_exec(command: &mut Command) {
    #[cfg(target_os = "linux")]
    // SAFETY: between fork and exec the hook makes one async-signal-safe
    // system call and allocates nothing.
    unsafe {
        command.pre_exec(mark_inherited_fds_close_on_exec);
    }
    #[cfg(not(target_os = "linux"))]
    let _ = command;
}

#[cfg(target_os = "linux")]
fn mark_inherited_fds_close_on_exec() -> std::io::Result<()> {
    const FIRST_INHERITED: libc::c_uint = 3;
    // SAFETY: close_range takes plain integers and only changes descriptor
    // flags of the calling process. rustix 1 does not wrap it.
    let result = unsafe {
        libc::syscall(
            libc::SYS_close_range,
            FIRST_INHERITED,
            libc::c_uint::MAX,
            libc::CLOSE_RANGE_CLOEXEC,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        // Reading errno neither allocates nor takes a lock.
        Err(std::io::Error::last_os_error())
    }
}

/// evorch's launch policy for every child that is not started on a
/// pseudo-terminal: [`detach_from_terminal`] and [`close_inherited_fds_on_exec`].
///
/// Sandboxed commands get it from [`crate::WrappedCommand::to_command`]. Tokio
/// commands apply it through `as_std_mut` and run as a [`GroupChild`], which
/// stops descendants too.
pub fn apply_launch_policy(command: &mut Command) {
    detach_from_terminal(command);
    close_inherited_fds_on_exec(command);
}

/// The process group of a child started with [`detach_from_terminal`], shared by
/// everything that may stop it.
///
/// The group id is the leader's pid, and the kernel keeps it reserved while the
/// leader exists, including as an exited but unreaped zombie. Once the leader
/// is reaped and its descendants are gone, the id can belong to an unrelated
/// group. So the group is only ever killed before its leader is reaped: reaping
/// goes through [`kill_and_reap`](Self::kill_and_reap), which kills what is
/// left of the group, reaps, and retires the handle under one lock. After that,
/// [`kill`](Self::kill) on any clone sends nothing.
#[derive(Debug, Clone)]
pub struct ProcessGroup {
    leader: Arc<Mutex<Option<Pid>>>,
}

impl ProcessGroup {
    /// The group led by the child with this pid (`Child::id`). `None`, as
    /// reported by an already reaped Tokio child, is a retired group.
    pub fn new(leader: Option<u32>) -> Self {
        let leader = leader
            .and_then(|pid| i32::try_from(pid).ok())
            .and_then(Pid::from_raw);
        Self {
            leader: Arc::new(Mutex::new(leader)),
        }
    }

    /// The group (and leader) id, until the leader is reaped.
    pub fn leader(&self) -> Option<Pid> {
        *self.lock()
    }

    /// Send SIGKILL to the whole group, unless its leader has been reaped.
    pub fn kill(&self) {
        if let Some(leader) = *self.lock() {
            let _ = rustix::process::kill_process_group(leader, Signal::KILL);
        }
    }

    /// Kill what is left of the group, then run `reap` on its exited leader.
    ///
    /// Unless `reap` reports the leader still running (`Ok(None)`), the handle
    /// retires: the reaped id is never signalled again. Call this only for an
    /// exited leader ([`has_exited`], [`wait_for_exit`]) unless the group is
    /// meant to be stopped.
    pub fn kill_and_reap<T>(
        &self,
        reap: impl FnOnce() -> io::Result<Option<T>>,
    ) -> io::Result<Option<T>> {
        let mut leader = self.lock();
        if let Some(pid) = *leader {
            let _ = rustix::process::kill_process_group(pid, Signal::KILL);
        }
        let reaped = reap();
        if !matches!(reaped, Ok(None)) {
            *leader = None;
        }
        reaped
    }

    /// Retire the handle without signalling, when the leader was reaped some
    /// other way or its state is unknown.
    pub fn release(&self) {
        *self.lock() = None;
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<Pid>> {
        self.leader.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Kills a [`ProcessGroup`] when dropped, unless its leader was reaped first.
#[derive(Debug)]
pub struct ProcessGroupGuard(ProcessGroup);

impl ProcessGroupGuard {
    pub const fn new(group: ProcessGroup) -> Self {
        Self(group)
    }
}

impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        self.0.kill();
    }
}

/// Whether the child `leader` has exited, leaving it unreaped.
///
/// # Errors
/// Fails when `leader` is not an unreaped child of this process.
pub fn has_exited(leader: Pid) -> io::Result<bool> {
    let options = WaitIdOptions::EXITED | WaitIdOptions::NOWAIT | WaitIdOptions::NOHANG;
    Ok(rustix::process::waitid(WaitId::Pid(leader), options)?.is_some())
}

/// Block until the child `leader` has exited, leaving it unreaped.
///
/// # Errors
/// Fails when `leader` is not an unreaped child of this process.
pub fn wait_for_exit(leader: Pid) -> io::Result<()> {
    loop {
        let options = WaitIdOptions::EXITED | WaitIdOptions::NOWAIT;
        match rustix::process::waitid(WaitId::Pid(leader), options) {
            Err(rustix::io::Errno::INTR) => {}
            result => return result.map(drop).map_err(io::Error::from),
        }
    }
}

/// A Tokio child started under the launch policy, which owns its process group.
///
/// Waiting notices the leader's exit without reaping it (Linux: a pidfd), kills
/// the group while the exited leader still reserves its id, which also stops
/// descendants holding the leader's pipes, and only then reaps the leader.
/// Dropping a child that was not reaped kills its group.
#[derive(Debug)]
pub struct GroupChild {
    child: tokio::process::Child,
    group: ProcessGroup,
    #[cfg(target_os = "linux")]
    exit: tokio::io::unix::AsyncFd<std::os::fd::OwnedFd>,
}

impl GroupChild {
    /// Spawn `command`, which must already carry the launch policy
    /// ([`apply_launch_policy`] or [`crate::WrappedCommand::to_command`]).
    ///
    /// # Errors
    /// Returns the spawn error, or the error of watching the started child,
    /// after its group was killed.
    pub fn spawn(command: &mut tokio::process::Command) -> io::Result<Self> {
        let mut child = command.spawn()?;
        let group = ProcessGroup::new(child.id());
        #[cfg(target_os = "linux")]
        let exit = match exit_notifier(&group) {
            Ok(exit) => exit,
            Err(error) => {
                group.kill();
                let _ = child.start_kill();
                return Err(error);
            }
        };
        Ok(Self {
            child,
            group,
            #[cfg(target_os = "linux")]
            exit,
        })
    }

    pub const fn take_stdin(&mut self) -> Option<tokio::process::ChildStdin> {
        self.child.stdin.take()
    }

    pub const fn take_stdout(&mut self) -> Option<tokio::process::ChildStdout> {
        self.child.stdout.take()
    }

    pub const fn take_stderr(&mut self) -> Option<tokio::process::ChildStderr> {
        self.child.stderr.take()
    }

    /// The child's process group, for stopping it from elsewhere.
    pub const fn group(&self) -> &ProcessGroup {
        &self.group
    }

    /// Kill the whole group, unless the leader has been reaped.
    pub fn kill(&self) {
        self.group.kill();
    }

    /// Resolves once the leader has exited; it stays unreaped, keeping the
    /// group id reserved. Cancel-safe.
    ///
    /// # Errors
    /// Fails when the exit cannot be observed.
    pub async fn exited(&self) -> io::Result<()> {
        #[cfg(target_os = "linux")]
        {
            // An exit is permanent, so the readiness is never cleared.
            self.exit.readable().await.map(drop)
        }
        #[cfg(not(target_os = "linux"))]
        loop {
            match self.group.leader() {
                Some(leader) if !has_exited(leader)? => {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
                _ => return Ok(()),
            }
        }
    }

    /// Wait for the leader to exit, kill what is left of its group, and reap
    /// the leader. Cancel-safe.
    ///
    /// # Errors
    /// Fails when the exit cannot be observed or the leader cannot be reaped.
    pub async fn wait(&mut self) -> io::Result<ExitStatus> {
        loop {
            self.exited().await?;
            let child = &mut self.child;
            let reaped = match self.group.leader() {
                Some(leader) => match has_exited(leader) {
                    Ok(true) => self.group.kill_and_reap(|| child.try_wait())?,
                    Ok(false) => None,
                    Err(error) => {
                        // Not an unreaped child any more: its id may be reused.
                        self.group.release();
                        return Err(error);
                    }
                },
                None => child.try_wait()?,
            };
            if let Some(status) = reaped {
                return Ok(status);
            }
            // Readiness without an exit is not expected; do not spin on it.
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    /// Kill the whole group, then reap the leader. Cancel-safe.
    ///
    /// # Errors
    /// Fails when the leader cannot be reaped.
    pub async fn kill_and_wait(&mut self) -> io::Result<ExitStatus> {
        self.kill();
        self.wait().await
    }
}

impl Drop for GroupChild {
    fn drop(&mut self) {
        // Tokio reaps a dropped child in the background; its group goes first.
        self.group.kill();
    }
}

#[cfg(target_os = "linux")]
fn exit_notifier(
    group: &ProcessGroup,
) -> io::Result<tokio::io::unix::AsyncFd<std::os::fd::OwnedFd>> {
    let leader = group
        .leader()
        .ok_or_else(|| io::Error::other("the started child has no pid"))?;
    // A pidfd turns readable when the process exits and stays so; unlike a
    // wait, observing it does not reap the process.
    let pidfd = rustix::process::pidfd_open(leader, rustix::process::PidfdFlags::empty())?;
    tokio::io::unix::AsyncFd::with_interest(pidfd, tokio::io::Interest::READABLE)
}
