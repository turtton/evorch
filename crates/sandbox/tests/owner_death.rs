//! What the guard does when the process that owns the children is killed.
//!
//! The harness (this test) starts the owner as a separate process, the way the
//! core is a separate process. The owner starts one child for each ledger kind
//! (through the async owner), a sandboxed session (the sync owner), and a child
//! and a session with no ledger at all: eleven roots. Every root has a
//! grandchild in the same group. The harness then kills the owner with SIGKILL,
//! so nothing in it runs again: no Drop, no reaper, no ledger.
//!
//! Every root and every grandchild must end, and an unrelated process that merely
//! lives beside them must not. The harness waits for each death on a pidfd (the
//! kernel's exit event); there is no sleep and no time limit in the test.
//!
//! This covers the typed owners every production caller goes through, one entry
//! per kind. It is not the real release core running its callers (that is B3).
#![cfg(target_os = "linux")]

use std::io::{BufRead, BufReader, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};

use sandbox::owned::{OwnedChild, OwnedCommand, OwnedSession};
use sandbox::{ChildKind, SpawnLedger};
use tokio::io::AsyncBufReadExt;

const REPORT: &str = "OWNED-ROOT";
const READY: &str = "OWNED-READY";
const OWNER_FLAG: &str = "SANDBOX_OWNER_DEATH_ROLE";
const TEST: &str = "a_killed_owner_takes_every_root_and_grandchild_with_it";
const KINDS: [ChildKind; 8] = [
    ChildKind::McpStdio,
    ChildKind::SandboxSession,
    ChildKind::PullRequestSource,
    ChildKind::ShellJob,
    ChildKind::Eval,
    ChildKind::Probe,
    ChildKind::Renderer,
    ChildKind::Other,
];

/// A root that leaves a grandchild in its own group, reports the grandchild's
/// pid, and then waits for it.
fn root() -> OwnedCommand {
    let mut command = OwnedCommand::new("/bin/sh");
    command
        .args(["-c", "sleep 3600 & echo $!; wait"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    command
}

/// `REPORT label root_pid grandchild_pid`, one line per root, for the harness (the
/// libtest header shares the stream, so the harness reads only prefixed lines).
fn report(label: &str, root: Option<u32>, grandchild: &str) {
    let mut out = std::io::stdout().lock();
    writeln!(
        out,
        "{REPORT} {label} {} {grandchild}",
        root.expect("a running root")
    )
    .unwrap();
    out.flush().unwrap();
}

/// The owner role: start the eleven roots, report them, and never return.
fn run_owner() -> ! {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let ledger = SpawnLedger::new();
        let mut keep = Vec::new();
        for kind in KINDS {
            let mut child = OwnedChild::spawn_blocking(Some(ledger.clone()), kind, None, root())
                .await
                .unwrap();
            let mut lines = tokio::io::BufReader::new(child.take_stdout().unwrap()).lines();
            let grandchild = lines.next_line().await.unwrap().unwrap();
            report(kind.as_str(), child.id(), &grandchild);
            keep.push((child, lines));
        }
        let mut session = {
            let admission = ledger.admit().unwrap();
            OwnedSession::spawn_in(&admission, ChildKind::SandboxSession, None, &mut root())
                .unwrap()
        };
        let mut lines = BufReader::new(session.take_stdout().unwrap());
        let mut grandchild = String::new();
        lines.read_line(&mut grandchild).unwrap();
        report("sync-session", session.id(), grandchild.trim());
        let mut bare = OwnedChild::spawn(&mut root()).unwrap();
        let mut bare_lines = tokio::io::BufReader::new(bare.take_stdout().unwrap()).lines();
        let grandchild = bare_lines.next_line().await.unwrap().unwrap();
        report("no-ledger-child", bare.id(), &grandchild);
        let mut bare_session = OwnedSession::spawn(&mut root()).unwrap();
        let mut bare_session_lines = BufReader::new(bare_session.take_stdout().unwrap());
        let mut grandchild = String::new();
        bare_session_lines.read_line(&mut grandchild).unwrap();
        report("no-ledger-session", bare_session.id(), grandchild.trim());
        println!("{READY}");
        std::io::stdout().flush().unwrap();
        // Owned until the harness kills this process.
        let _owners = (keep, session, bare, bare_session, lines, bare_lines);
        std::future::pending::<()>().await;
    });
    unreachable!("the owner never finishes");
}

/// Resolves once `pid` has exited (a zombie counts), on its pidfd.
fn death_event(pid: i32) -> OwnedFd {
    process_guard::pidfd_open(pid).expect("the process exists while the owner lives")
}

fn wait_for_death(fd: &OwnedFd) {
    process_guard::wire::poll(
        &mut [process_guard::wire::fd(fd.as_raw_fd(), libc::POLLIN)],
        None,
    )
    .unwrap();
}

/// Kills its process when dropped, so a failed assertion leaves nothing behind (for the
/// owner, the guard then ends its roots).
struct Bystander(std::process::Child);
impl Drop for Bystander {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn running(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
        stat.rsplit_once(')')
            .is_some_and(|(_, rest)| !rest.trim_start().starts_with('Z'))
    })
}

// Given: an owner holding eleven roots (eight ledger kinds, a sandbox session, and a
// child and a session with no ledger), each with a same-group grandchild, and an
// unrelated process beside them / When: the owner is killed with SIGKILL / Then: every
// root and grandchild ends, and the unrelated process lives on.
#[test]
fn a_killed_owner_takes_every_root_and_grandchild_with_it() {
    if std::env::var_os(OWNER_FLAG).is_some() {
        run_owner();
    }
    let mut unrelated = Bystander(
        Command::new("sleep")
            .arg("3600")
            .process_group(0)
            .stdin(Stdio::null())
            .spawn()
            .expect("start the unrelated process"),
    );
    let mut owner = Bystander(
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", TEST, "--nocapture"])
            .env(OWNER_FLAG, "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("start the owner"),
    );
    let mut lines = BufReader::new(owner.0.stdout.take().unwrap()).lines();
    let mut watched = Vec::new();
    let mut labels = Vec::new();
    loop {
        let line = lines.next().expect("the owner reports").unwrap();
        if line == READY {
            break;
        }
        let Some(report) = line.strip_prefix(REPORT) else {
            continue;
        };
        let [label, root, grandchild] = report.split_whitespace().collect::<Vec<_>>()[..] else {
            panic!("unexpected report: {line}");
        };
        labels.push(label.to_owned());
        for pid in [root, grandchild] {
            watched.push((format!("{label} {pid}"), pid.parse::<i32>().unwrap()));
        }
    }
    assert_eq!(labels.len(), 11, "{labels:?}");
    for kind in KINDS {
        assert!(
            labels.iter().any(|label| label == kind.as_str()),
            "{kind:?}"
        );
    }
    // Hold each process by its pidfd before the kill, so a death cannot be missed
    // and a pid cannot be reused under the wait.
    let events: Vec<_> = watched
        .iter()
        .map(|(name, pid)| (name.clone(), death_event(*pid)))
        .collect();
    for (name, pid) in &watched {
        assert!(
            running(u32::try_from(*pid).unwrap()),
            "{name} is not running"
        );
    }

    owner.0.kill().expect("kill the owner");
    owner.0.wait().expect("reap the owner");

    for (name, event) in &events {
        wait_for_death(event);
        eprintln!("{name} ended");
    }
    assert!(
        running(unrelated.0.id()),
        "an unrelated process must not be signalled"
    );
    assert!(unrelated.0.try_wait().unwrap().is_none());
}
