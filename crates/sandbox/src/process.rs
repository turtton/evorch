//! How evorch starts child processes: detached from its terminal, without its
//! descriptors, and stoppable together with their descendants.
//!
//! evorch may run from a terminal, and it holds descriptors it never meant to
//! share: the GUI's browser engine keeps databases, GPU buffers and IPC sockets
//! open without `O_CLOEXEC`, and a bubblewrap sandbox passes them on to the
//! sandboxed command. [`apply_launch_policy`] starts a child in its own session
//! and lets the program it executes inherit only stdin, stdout and stderr.
//! [`ProcessGroupGuard`] then stops the child with its descendants.
//!
//! These are pre-exec hooks, so the standard library forks the parent instead
//! of using `posix_spawn`; a large parent pays for copying its page tables on
//! every launch. Pseudo-terminal launches (`portable-pty`) already start a new
//! session and close inherited descriptors, and do not need them.

use std::os::unix::process::CommandExt;
use std::process::Command;

use rustix::process::{Pid, Signal};

/// Start the child in a new session, which is also a new process group.
///
/// A child left in evorch's session sits in a background process group of
/// evorch's controlling terminal: reading `/dev/tty` (a git credential or ssh
/// password prompt) stops it with SIGTTIN, and it never finishes. Without a
/// controlling terminal, opening `/dev/tty` fails and the child reports the
/// error instead. The session and process group ids both equal the child's
/// pid, so [`ProcessGroupGuard`] reaches the child and every descendant that
/// stays in its group.
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
/// commands apply it through `as_std_mut`. Stop the child with
/// [`ProcessGroupGuard`] to stop its descendants too.
pub fn apply_launch_policy(command: &mut Command) {
    detach_from_terminal(command);
    close_inherited_fds_on_exec(command);
}

/// Kills the process group of a child started with [`detach_from_terminal`]
/// when dropped, or earlier on [`kill`](Self::kill).
///
/// The group id is the child's pid. Killing it stops the child and every
/// descendant that did not leave the group, including descendants that still
/// hold the child's pipes after it exited. Kill before or right after reaping
/// the child: once the group is empty its id can be reused.
#[derive(Debug)]
pub struct ProcessGroupGuard {
    leader: Option<Pid>,
}

impl ProcessGroupGuard {
    /// Guard the group led by the child with this pid (`Child::id`). `None`,
    /// as reported by an already reaped Tokio child, guards nothing.
    pub fn new(leader: Option<u32>) -> Self {
        Self {
            leader: leader
                .and_then(|pid| i32::try_from(pid).ok())
                .and_then(Pid::from_raw),
        }
    }

    /// The group (and leader) id, if there is one.
    pub const fn leader(&self) -> Option<Pid> {
        self.leader
    }

    /// Send SIGKILL to the whole group. A group that is already gone is not an error.
    pub fn kill(&self) {
        if let Some(leader) = self.leader {
            let _ = rustix::process::kill_process_group(leader, Signal::KILL);
        }
    }
}

impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        self.kill();
    }
}
