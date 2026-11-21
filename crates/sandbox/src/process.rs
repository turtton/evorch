//! The raw process-group backend that survives the typed owner API.
//!
//! Linux non-PTY children are owned by [`crate::owned`] (an authenticated process
//! guard, with or without a ledger); nothing here starts one. What remains is the
//! boundary B3 replaces: [`ProcessGroup`] / [`ProcessGroupGuard`] /
//! [`wait_for_exit`] / [`has_exited`] for a pseudo-terminal child (the shell's PTY
//! jobs, started by `portable-pty`, which already starts its own session and closes
//! inherited descriptors), registered through
//! [`crate::ledger::Admission::register_pty_group`], and
//! [`close_inherited_fds_on_exec`], a descriptor-hygiene hook for a std command that
//! a test fixture starts and supervises itself (it owns nothing).
//!
//! The non-Linux owner adapter keeps its previous backend privately
//! (`GroupChild`, the launch policy); it carries no new D-7 claim.

use std::io;
use std::os::unix::process::CommandExt;
#[cfg(not(target_os = "linux"))]
use std::process::Command;
#[cfg(not(target_os = "linux"))]
use std::process::ExitStatus;
use std::sync::{Arc, Mutex, PoisonError};

use rustix::process::{Pid, Signal, WaitId, WaitIdOptions};

#[cfg(not(target_os = "linux"))]
use crate::ledger::ChildEntry;

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
#[cfg(not(target_os = "linux"))]
pub(crate) fn detach_from_terminal(command: &mut Command) {
    // SAFETY: between fork and exec the hook makes one async-signal-safe
    // system call and allocates nothing.
    unsafe {
        command.pre_exec(|| {
            rustix::process::setsid()?;
            // Core's signalfd mask is process-local, not child policy. Normal
            // tools must remain stoppable by TERM/INT. The handoff exec path
            // deliberately does not use this ordinary child launch hook.
            let mut shutdown: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut shutdown);
            libc::sigaddset(&mut shutdown, libc::SIGINT);
            libc::sigaddset(&mut shutdown, libc::SIGTERM);
            if libc::sigprocmask(libc::SIG_UNBLOCK, &shutdown, std::ptr::null_mut()) != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

/// Close every descriptor above stderr when the child executes its program.
///
/// Not an ownership API: it only marks descriptors for a std command that a test
/// fixture starts and supervises itself. Product children are started through
/// [`crate::owned::OwnedCommand`], whose guard helper applies the same policy.
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
pub fn close_inherited_fds_on_exec(command: &mut std::process::Command) {
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

/// evorch's launch policy for the non-Linux adapter's children.
#[cfg(not(target_os = "linux"))]
pub(crate) fn apply_launch_policy(command: &mut Command) {
    detach_from_terminal(command);
    close_inherited_fds_on_exec(command);
}

/// The process group of a child that leads its own session, shared by everything
/// that may stop it.
///
/// The group id is the leader's pid, and the kernel keeps it reserved while the
/// leader exists, including as an exited but unreaped zombie. Once the leader
/// is reaped and its descendants are gone, the id can belong to an unrelated
/// group. So the group is only ever killed before its leader is reaped: reaping
/// goes through [`kill_and_reap`](Self::kill_and_reap), which kills what is
/// left of the group, reaps, and retires the handle under one lock, and a
/// leader left to another reaper is first stopped and retired with
/// [`kill_and_release`](Self::kill_and_release). After that,
/// [`kill`](Self::kill) on any clone sends nothing.
///
/// # Only a pseudo-terminal child can lead one
///
/// Linux children that are not on a pseudo-terminal are owned by
/// [`crate::owned`]; this raw handle is the PTY boundary B3 replaces. A live
/// group comes only from a command started on a pseudo-terminal slave:
///
/// ```no_run
/// fn start(
///     slave: &dyn portable_pty::SlavePty,
/// ) -> std::io::Result<sandbox::process::ProcessGroup> {
///     let command = portable_pty::CommandBuilder::new("true");
///     sandbox::process::ProcessGroup::spawn_pty(slave, command).map(|(_child, group)| group)
/// }
/// ```
///
/// A bare pid has no constructor:
///
/// ```compile_fail,E0624
/// let child = std::process::Command::new("true").spawn().unwrap();
/// let _ = sandbox::process::ProcessGroup::new(Some(child.id()));
/// ```
///
/// and a std or Tokio child is not a pseudo-terminal slave:
///
/// ```compile_fail,E0277
/// let child = std::process::Command::new("true").spawn().unwrap();
/// let _ = sandbox::process::ProcessGroup::spawn_pty(&child, portable_pty::CommandBuilder::new("true"));
/// ```
///
/// ```compile_fail,E0277
/// async fn start() {
///     let child = tokio::process::Command::new("true").spawn().unwrap();
///     let _ = sandbox::process::ProcessGroup::spawn_pty(&child, portable_pty::CommandBuilder::new("true"));
/// }
/// ```
#[derive(Debug, Clone)]
pub struct ProcessGroup {
    leader: Arc<Mutex<Option<Pid>>>,
}

impl ProcessGroup {
    /// Start `command` on the pseudo-terminal `slave` and take the group it
    /// leads. portable-pty starts the child in a session of its own, so its pid
    /// is the group id.
    ///
    /// The only way to obtain a live group: there is no constructor from a bare
    /// pid, and a std or Tokio child cannot be passed (`portable_pty::Child` is
    /// implemented for `std::process::Child`, so the child itself would prove
    /// nothing; the pseudo-terminal slave that starts it does).
    ///
    /// # Errors
    /// The error of the pseudo-terminal's spawn.
    pub fn spawn_pty(
        slave: &dyn portable_pty::SlavePty,
        command: portable_pty::CommandBuilder,
    ) -> io::Result<(Box<dyn portable_pty::Child + Send + Sync>, Self)> {
        let child = slave.spawn_command(command).map_err(io::Error::other)?;
        let group = Self::new(child.process_id());
        Ok((child, group))
    }

    /// A group with no leader: it signals nothing and is already retired.
    pub fn retired() -> Self {
        Self::new(None)
    }

    /// The group led by the child with this pid (`Child::id`). `None`, as
    /// reported by an already reaped Tokio child, is a retired group.
    pub(crate) fn new(leader: Option<u32>) -> Self {
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

    /// Send SIGTERM to the whole group while the leader still reserves its id.
    pub fn terminate(&self) {
        if let Some(leader) = *self.lock() {
            let _ = rustix::process::kill_process_group(leader, Signal::TERM);
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

    /// Kill the whole group a last time and retire the handle, before the
    /// leader is handed to a reaper that cannot retire it. Once SIGKILL has
    /// reached the group, nothing in it needs another signal.
    pub fn kill_and_release(&self) {
        let mut leader = self.lock();
        if let Some(pid) = leader.take() {
            let _ = rustix::process::kill_process_group(pid, Signal::KILL);
        }
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

#[cfg(not(target_os = "linux"))]
pub(crate) use legacy::GroupChild;

/// The non-Linux adapter's Tokio child. Kept private; Linux owners are guard-backed.
#[cfg(not(target_os = "linux"))]
mod legacy {
    use super::{ChildEntry, ExitStatus, ProcessGroup, has_exited, io, wait_for_exit};

    #[derive(Debug)]
    pub(crate) struct GroupChild {
        /// `None` only while `Drop` hands the child to the ledger's reaper.
        child: Option<tokio::process::Child>,
        group: ProcessGroup,
        entry: Option<ChildEntry>,
    }

    impl GroupChild {
        pub(crate) fn spawn(command: &mut tokio::process::Command) -> io::Result<Self> {
            let child = command.spawn()?;
            let group = ProcessGroup::new(child.id());
            Ok(Self {
                child: Some(child),
                group,
                entry: None,
            })
        }

        pub(crate) fn attach(&mut self, entry: ChildEntry) {
            self.entry = Some(entry);
        }

        fn note_reaped(&mut self) {
            if let Some(entry) = self.entry.take() {
                entry.reaped();
            }
        }

        pub(crate) fn take_stdin(&mut self) -> Option<tokio::process::ChildStdin> {
            self.child.as_mut().and_then(|child| child.stdin.take())
        }

        pub(crate) fn take_stdout(&mut self) -> Option<tokio::process::ChildStdout> {
            self.child.as_mut().and_then(|child| child.stdout.take())
        }

        pub(crate) fn take_stderr(&mut self) -> Option<tokio::process::ChildStderr> {
            self.child.as_mut().and_then(|child| child.stderr.take())
        }

        pub(crate) fn id(&self) -> Option<u32> {
            self.child.as_ref().and_then(tokio::process::Child::id)
        }

        pub(crate) const fn group(&self) -> &ProcessGroup {
            &self.group
        }

        pub(crate) fn kill(&self) {
            self.group.kill();
        }

        pub(crate) async fn exited(&self) -> io::Result<()> {
            loop {
                match self.group.leader() {
                    Some(leader) if !has_exited(leader)? => {
                        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    }
                    _ => return Ok(()),
                }
            }
        }

        pub(crate) async fn wait(&mut self) -> io::Result<ExitStatus> {
            loop {
                self.exited().await?;
                let child = &mut self.child;
                let reaped = match self.group.leader() {
                    Some(leader) => match has_exited(leader) {
                        Ok(true) => self.group.kill_and_reap(|| try_wait(child))?,
                        Ok(false) => None,
                        Err(error) => {
                            self.group.release();
                            return Err(error);
                        }
                    },
                    None => try_wait(child)?,
                };
                if let Some(status) = reaped {
                    self.note_reaped();
                    return Ok(status);
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        }

        pub(crate) fn kill_and_reap_blocking(&mut self) -> io::Result<()> {
            self.kill();
            if let Some(leader) = self.group.leader() {
                wait_for_exit(leader)?;
                let child = &mut self.child;
                self.group.kill_and_reap(|| try_wait(child))?;
            }
            self.note_reaped();
            Ok(())
        }

        pub(crate) async fn kill_and_wait(&mut self) -> io::Result<ExitStatus> {
            self.kill();
            self.wait().await
        }
    }

    fn try_wait(child: &mut Option<tokio::process::Child>) -> io::Result<Option<ExitStatus>> {
        child
            .as_mut()
            .map_or(Ok(None), tokio::process::Child::try_wait)
    }

    impl Drop for GroupChild {
        fn drop(&mut self) {
            self.group.kill_and_release();
            if let (Some(entry), Some(child)) = (self.entry.take(), self.child.take()) {
                entry.reap_in_background(child);
            }
        }
    }
}

/// Pid of a Tokio child that has not been reaped.
#[cfg(not(target_os = "linux"))]
pub(crate) fn unreaped_pid(child: &tokio::process::Child) -> Option<Pid> {
    child
        .id()
        .and_then(|pid| i32::try_from(pid).ok())
        .and_then(Pid::from_raw)
}
