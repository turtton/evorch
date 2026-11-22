//! Isolated controller for the real GuardLost restart regression. No synthetic
//! ledger receipts and no process-wide subreaper setting in the test runner.
use rustix::process::{self, Pid, PidfdFlags, Signal, WaitId, WaitIdOptions};
use sandbox::{SpawnLedger, ledger::ChildKind, owned::OwnedCommand};
use std::{
    os::fd::{AsFd, OwnedFd},
    process::{Command, Stdio},
};

pub(super) fn controller() -> bool {
    if std::env::var_os("B3_05_GUARD_LOST_CONTROLLER").is_some() {
        process::set_child_subreaper(Some(process::getpid())).unwrap();
        return true;
    }
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "restart::tests::finalization::m2::unconfirmed_reap_blocks_worker_join_and_storage_drain", "--test-threads=1"])
        .env("B3_05_GUARD_LOST_CONTROLLER", "1")
        .output().unwrap();
    assert!(
        output.status.success(),
        "controller: {:?}\n{}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    false
}
fn pin(pid: u32) -> OwnedFd {
    process::pidfd_open(Pid::from_raw(pid as i32).unwrap(), PidfdFlags::empty()).unwrap()
}
fn wait(fd: &OwnedFd, options: WaitIdOptions) -> rustix::io::Result<Option<process::WaitIdStatus>> {
    loop {
        match process::waitid(WaitId::PidFd(fd.as_fd()), options) {
            Err(rustix::io::Errno::INTR) => continue,
            result => return result,
        }
    }
}
pub(super) struct Family {
    root: OwnedFd,
    guard: OwnedFd,
}
impl Drop for Family {
    fn drop(&mut self) {
        // pidfds were acquired while both peers were alive. Even an assertion
        // failure kills exactly this family, waits for adoption, then reaps root.
        let _ = process::pidfd_send_signal(&self.root, Signal::KILL);
        let _ = process::pidfd_send_signal(&self.guard, Signal::KILL);
        let _ = wait(&self.guard, WaitIdOptions::EXITED | WaitIdOptions::NOWAIT);
        let reaped = wait(&self.root, WaitIdOptions::EXITED);
        if !std::thread::panicking() {
            assert!(reaped.unwrap().is_some(), "controller did not reap target");
        }
    }
}
pub(super) async fn lose_guard(ledger: &SpawnLedger) -> Family {
    use tokio::io::{AsyncBufReadExt, BufReader};
    let mut command = OwnedCommand::new("/bin/sh");
    command
        .args(["-c", "echo $PPID; read gate"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = ledger
        .admit()
        .unwrap()
        .spawn_owned(ChildKind::Other, None, &mut command)
        .unwrap();
    let root = pin(child.id().unwrap());
    let mut output = BufReader::new(child.take_stdout().unwrap()).lines();
    let guard: u32 = output.next_line().await.unwrap().unwrap().parse().unwrap();
    // The helper cannot finish while root is gated on its still-open stdin.
    let guard = pin(guard);
    wait(
        &guard,
        WaitIdOptions::EXITED | WaitIdOptions::NOWAIT | WaitIdOptions::NOHANG,
    )
    .unwrap();
    let family = Family { root, guard };
    let stdin = child.take_stdin().unwrap(); // EOF must not win against guard loss.
    process::pidfd_send_signal(&family.guard, Signal::KILL).unwrap();
    assert!(
        child
            .wait()
            .await
            .unwrap_err()
            .to_string()
            .contains("GuardLost")
    );
    let id = ledger.children()[0].id;
    drop(child);
    assert!(ledger.reap_child(id).await.is_err());
    // GuardLost is not death evidence. The controller observes target death
    // through P_PIDFD before dropping stdin or testing finalization.
    wait(&family.guard, WaitIdOptions::EXITED | WaitIdOptions::NOWAIT).ok();
    wait(&family.root, WaitIdOptions::EXITED | WaitIdOptions::NOWAIT).unwrap();
    drop(stdin);
    family
}
