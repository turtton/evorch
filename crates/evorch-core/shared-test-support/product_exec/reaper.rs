//! The reaper: a process of its own for each supervised family. It is the test
//! binary run again through its `f4p_family_reaper` role (the including test
//! file declares that test and calls [`run`]).
//!
//! Why a process per family. A descendant that outlives its parent is adopted by
//! the nearest living subreaper. With one subreaper per test process (the
//! harness), the orphans of every family land in the same place and nothing
//! says whose they are. Here the reaper is the subreaper of exactly one family:
//! it starts the leader as its only child, so every other child it ever has is
//! an orphan of that leader's tree. Ownership is therefore a fact of the
//! process tree, not a guess from a command line, a working directory, a time
//! or a path. The harness itself is not a subreaper and never signals a
//! process it found by looking: it asks the reaper, which only ever signals
//! its own direct children.
//!
//! Protocol (lines on the reaper's stdin and stdout, prefix `F4P-R `):
//! * stdout `LEADER {pid, stdin_fd, stdout_fd, stderr_fd}`: the leader runs. The
//!   harness reopens the three pipes through `/proc/<reaper>/fd/<n>` and sends
//!   `ACK`, after which the reaper closes its own ends, so the leader's pipes
//!   end exactly when the leader's tree ends.
//! * stdout `EXITED <raw wait status>`: the leader exited. It stays a zombie
//!   (its pid and group id reserved) until `REAP`.
//! * stdin `SIGNAL <n>`, `SNAPSHOT`, `JOIN`, `KILL`, `REAP`; replies `SNAPSHOT`,
//!   `JOINED`, `KILLED`, `REAPED`.
//! * End of stdin (the harness died or dropped the family) does `KILL`, `REAP`
//!   and exits.
use super::observe;
use serde::{Deserialize, Serialize};
use std::{
    io::{BufRead, Write},
    os::{
        fd::{AsFd, AsRawFd},
        unix::process::{CommandExt, ExitStatusExt},
    },
    process::{Command, ExitStatus, Stdio},
    sync::Arc,
};

pub const ROLE_TEST: &str = "f4p_family_reaper";
pub const ENV_SPEC: &str = "EVORCH_F4P_REAPER_SPEC";
pub const EVENT_PREFIX: &str = "F4P-R ";

/// What the reaper starts as the leader. The environment is complete: the
/// leader gets exactly these variables.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Spec {
    pub program: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: Option<String>,
}

fn emit(kind: &str, data: serde_json::Value) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(
        out,
        "{EVENT_PREFIX}{}",
        serde_json::json!({"kind": kind, "data": data})
    );
    let _ = out.flush();
}

pub fn raw_status(status: &rustix::process::WaitIdStatus) -> i32 {
    if let Some(code) = status.exit_status() {
        (code & 0xff) << 8
    } else {
        let signal = status.terminating_signal().unwrap_or(0) & 0x7f;
        if status.dumped() {
            signal | 0x80
        } else {
            signal
        }
    }
}

fn wait_pidfd(
    fd: &impl AsFd,
    options: rustix::process::WaitIdOptions,
) -> Option<rustix::process::WaitIdStatus> {
    loop {
        match rustix::process::waitid(rustix::process::WaitId::PidFd(fd.as_fd()), options) {
            Err(rustix::io::Errno::INTR) => continue,
            Ok(status) => return status,
            Err(_) => return None,
        }
    }
}

/// Direct children of this process other than the leader.
fn other_children(leader: i32) -> Vec<observe::ProcInfo> {
    let me = std::process::id() as i32;
    observe::all_processes()
        .into_iter()
        .filter(|p| p.ppid == me && p.pid != leader)
        .collect()
}

/// Kills and reaps one direct child. The child is unreaped, so its pid cannot
/// have been reused; the pidfd makes that explicit.
fn kill_child(pid: i32) {
    let Some(raw) = rustix::process::Pid::from_raw(pid) else {
        return;
    };
    let Ok(fd) = rustix::process::pidfd_open(raw, rustix::process::PidfdFlags::empty()) else {
        return;
    };
    let _ = rustix::process::pidfd_send_signal(&fd, rustix::process::Signal::KILL);
    let _ = wait_pidfd(&fd, rustix::process::WaitIdOptions::EXITED);
}

/// Kills the leader's group and the leader, waits for the leader's exit (which
/// hands its children to this process), then kills and reaps every other child
/// until there is none. When the list is empty nothing of the family can be
/// alive: a living descendant would have a living parent chain up to a child of
/// this process, or would have been adopted by it.
fn kill_phase(leader: i32, pidfd: &impl AsFd) -> Vec<observe::ProcInfo> {
    if let Some(raw) = rustix::process::Pid::from_raw(leader) {
        let _ = rustix::process::kill_process_group(raw, rustix::process::Signal::KILL);
    }
    let _ = rustix::process::pidfd_send_signal(pidfd.as_fd(), rustix::process::Signal::KILL);
    let _ = wait_pidfd(
        pidfd,
        rustix::process::WaitIdOptions::EXITED | rustix::process::WaitIdOptions::NOWAIT,
    );
    let mut killed = Vec::new();
    loop {
        let children = other_children(leader);
        if children.is_empty() {
            return killed;
        }
        for child in children {
            kill_child(child.pid);
            killed.push(child);
        }
    }
}

/// Runs the reaper role. Never returns normally: it exits the process.
pub fn run() -> ! {
    let spec: Spec = serde_json::from_str(
        &std::env::var(ENV_SPEC).unwrap_or_else(|_| panic!("{ENV_SPEC} is not set")),
    )
    .expect("a reaper spec");
    rustix::process::set_child_subreaper(Some(rustix::process::getpid()))
        .expect("become the family's subreaper");
    let mut command = Command::new(&spec.program);
    command
        .args(&spec.args)
        .env_clear()
        .envs(spec.env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    if let Some(cwd) = &spec.cwd {
        command.current_dir(cwd);
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            emit("SPAWN_FAILED", serde_json::json!(error.to_string()));
            std::process::exit(0);
        }
    };
    let leader = child.id() as i32;
    let pidfd = Arc::new(
        rustix::process::pidfd_open(
            rustix::process::Pid::from_raw(leader).expect("a child pid"),
            rustix::process::PidfdFlags::empty(),
        )
        .expect("pidfd of the leader"),
    );
    let (leader_in, leader_out, leader_err) = (
        child.stdin.take().expect("piped"),
        child.stdout.take().expect("piped"),
        child.stderr.take().expect("piped"),
    );
    emit(
        "LEADER",
        serde_json::json!({
            "pid": leader,
            "stdin_fd": leader_in.as_raw_fd(),
            "stdout_fd": leader_out.as_raw_fd(),
            "stderr_fd": leader_err.as_raw_fd(),
        }),
    );
    let stdin = std::io::stdin();
    let mut control = stdin.lock().lines();
    let mut acked = false;
    // The ACK is the first line. Anything else (or EOF) means the harness
    // gave up on this family before it started.
    if matches!(control.next(), Some(Ok(line)) if line == "ACK") {
        acked = true;
    }
    drop((leader_in, leader_out, leader_err));
    if acked {
        let pidfd = pidfd.clone();
        std::thread::spawn(move || {
            let status = wait_pidfd(
                &*pidfd,
                rustix::process::WaitIdOptions::EXITED | rustix::process::WaitIdOptions::NOWAIT,
            );
            emit(
                "EXITED",
                serde_json::json!(status.as_ref().map_or(0, raw_status)),
            );
        });
    }
    let mut reaped = false;
    if acked {
        while let Some(Ok(line)) = control.next() {
            let (word, argument) = line.split_once(' ').unwrap_or((line.as_str(), ""));
            match word {
                "SIGNAL" => {
                    if let Some(signal) = argument
                        .parse::<i32>()
                        .ok()
                        .and_then(rustix::process::Signal::from_named_raw)
                    {
                        let _ = rustix::process::pidfd_send_signal(&*pidfd, signal);
                    }
                }
                "SNAPSHOT" => {
                    let me = std::process::id() as i32;
                    let alive: Vec<_> = observe::descendants(me)
                        .into_iter()
                        .filter(|p| p.pid != leader && p.state != 'Z')
                        .collect();
                    emit("SNAPSHOT", serde_json::json!(alive));
                }
                "JOIN" => {
                    // In a thread: a KILL must be able to interrupt a wait on a
                    // process that never exits.
                    std::thread::spawn(move || {
                        let mut joined = Vec::new();
                        loop {
                            let children = other_children(leader);
                            if children.is_empty() {
                                break;
                            }
                            for child in children {
                                let Some(raw) = rustix::process::Pid::from_raw(child.pid) else {
                                    continue;
                                };
                                let Ok(fd) = rustix::process::pidfd_open(
                                    raw,
                                    rustix::process::PidfdFlags::empty(),
                                ) else {
                                    continue;
                                };
                                if let Some(status) =
                                    wait_pidfd(&fd, rustix::process::WaitIdOptions::EXITED)
                                {
                                    joined.push((child.pid, raw_status(&status)));
                                }
                            }
                        }
                        emit("JOINED", serde_json::json!(joined));
                    });
                }
                "KILL" => {
                    let killed = kill_phase(leader, &*pidfd);
                    emit("KILLED", serde_json::json!(killed));
                }
                "REAP" => {
                    let status = child.wait().expect("reap the leader");
                    emit("REAPED", serde_json::json!(status.into_raw()));
                    reaped = true;
                    break;
                }
                _ => (),
            }
        }
    }
    if !reaped {
        // The harness is gone, or never acknowledged: the family goes with it.
        let _ = kill_phase(leader, &*pidfd);
        let status: ExitStatus = child.wait().unwrap_or_else(|_| ExitStatus::from_raw(0));
        emit("REAPED", serde_json::json!(status.into_raw()));
    }
    std::process::exit(0);
}
