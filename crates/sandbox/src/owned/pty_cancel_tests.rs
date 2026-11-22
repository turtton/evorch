//! Paused before registration, after actual target READY, not a launch stub.
use super::*;
use crate::ledger::SpawnLedger;
use std::{
    io::{BufRead, BufReader},
    os::fd::{AsRawFd, OwnedFd},
    process::Command,
};
const TEST: &str = "owned::linux::pty::tests::cancel_and_registration_unwind_keep_ownership";
#[derive(Debug)]
struct Reaper(Vec<(i32, OwnedFd)>);
impl Drop for Reaper {
    fn drop(&mut self) {
        for (_, fd) in &self.0 {
            unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    fd.as_raw_fd(),
                    libc::SIGKILL,
                    std::ptr::null::<libc::siginfo_t>(),
                    0,
                );
            }
        }
        for (pid, _) in &self.0 {
            loop {
                if unsafe { libc::waitpid(*pid, std::ptr::null_mut(), 0) } >= 0
                    || std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted
                {
                    break;
                }
            }
        }
    }
}
#[test]
fn cancel_and_registration_unwind_keep_ownership() {
    let Ok(action) = std::env::var("PTY_CANCEL_CONTROLLER") else {
        for action in ["cancel", "registration-unwind"] {
            let result = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", TEST, "--nocapture", "--test-threads=1"])
                .env("PTY_CANCEL_CONTROLLER", action)
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "{action}: {}\n{}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            );
        }
        return;
    };
    assert_eq!(unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1) }, 0);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .max_blocking_threads(1)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let ledger = SpawnLedger::new();
        let work_ledger = ledger.clone();
        let (started, ready) = tokio::sync::oneshot::channel();
        let (release, gate) = std::sync::mpsc::channel();
        let panic = action == "registration-unwind";
        let work = tokio::spawn(crate::owned::offload(move || {
            let admission = work_ledger.admit().unwrap();
            let mut command = OwnedPtyCommand::new("/bin/sh");
            command.args(["-c", "trap '' HUP; echo READY; read value"]);
            let (pending, master) = OwnedPtyChild::unregistered(
                &command,
                portable_pty::PtySize {
                    rows: 24,
                    cols: 80,
                    pixel_width: 0,
                    pixel_height: 0,
                },
            )?;
            let group = pending.0.as_ref().unwrap().group.clone();
            let root = group.id().unwrap() as i32;
            let helper = group.lock().child.as_ref().unwrap().id() as i32;
            let mut output = BufReader::new(master.try_clone_reader()?);
            let mut line = String::new();
            assert!(output.read_line(&mut line)? > 0);
            assert_eq!(line.trim(), "READY");
            // Capture identity while READY and the registration gate keep both
            // peers alive. Cleanup must never reopen a possibly reused PID.
            let death = process_guard::pidfd_open(root)?;
            let reaper = Reaper(vec![
                (root, death.try_clone()?),
                (helper, process_guard::pidfd_open(helper)?),
            ]);
            eprintln!("PTY_READY before registration root={root} helper={helper}");
            started.send((master, group, death, reaper)).unwrap();
            gate.recv().unwrap();
            assert!(!panic, "simulated registration unwind");
            let child = pending.register(&admission, ChildKind::ShellJob, None);
            Ok(child)
        }));
        let (_master, group, death, _reaper) = ready.await.unwrap();
        assert_eq!(ledger.child_stats().registered, 0);
        if !panic {
            work.abort();
            assert!(work.await.err().unwrap().is_cancelled());
            release.send(()).unwrap();
        } else {
            release.send(()).unwrap();
            assert!(work.await.unwrap().is_err());
        }
        // Only one Tokio blocking thread: this marker runs after the abandoned
        // launch has returned and Tokio has dropped its undeliverable owner.
        tokio::task::spawn_blocking(|| {}).await.unwrap();
        assert!(
            group.lock().child.is_none(),
            "abandoned launch kept guard instead of handing it to RAII cleanup"
        );
        if !panic {
            ledger.reap_all_children().await.unwrap();
            assert_eq!(ledger.child_stats().reaped, 1);
        } else {
            assert_eq!(ledger.child_stats().registered, 0);
        }
        process_guard::wire::poll(
            &mut [process_guard::wire::fd(death.as_raw_fd(), libc::POLLIN)],
            None,
        )
        .unwrap();
        assert_eq!(ledger.child_stats().unconfirmed, 0);
    });
}
