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
//!   `JOINING` (the JOIN has pinned its first round of children), `JOINED`,
//!   `KILLED`, `REAPED`.
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

/// Every reap of a child other than the leader, and every conversion of such a
/// child's pid into a pidfd that is then signalled, happens with this lock
/// held. A pid is read from the table and pinned by a pidfd inside one critical
/// section, and nothing else can reap (and so free the pid) inside it: a pid
/// that is listed and then opened is still the same, unreaped child.
static CHILDREN: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Pins a direct child by pidfd. The caller holds `CHILDREN`. The kernel
/// confirms that the pidfd names a child of this process before it is used.
fn pin_child(pid: i32) -> Option<std::os::fd::OwnedFd> {
    let raw = rustix::process::Pid::from_raw(pid)?;
    let fd = rustix::process::pidfd_open(raw, rustix::process::PidfdFlags::empty()).ok()?;
    match rustix::process::waitid(
        rustix::process::WaitId::PidFd(fd.as_fd()),
        rustix::process::WaitIdOptions::EXITED
            | rustix::process::WaitIdOptions::NOWAIT
            | rustix::process::WaitIdOptions::NOHANG,
    ) {
        // A child (running: `None`, or exited: a status). ECHILD means it is
        // not ours, and then nothing may be signalled.
        Ok(_) => Some(fd),
        Err(_) => None,
    }
}

/// The wait status of every child that was reaped, by pid, so that whoever
/// waits for a child can still learn how it ended when someone else reaped it
/// first. Touched only with `CHILDREN` held.
static REAPED: std::sync::Mutex<std::collections::BTreeMap<i32, i32>> =
    std::sync::Mutex::new(std::collections::BTreeMap::new());

/// Reaps a pinned child (the caller holds `CHILDREN`) and records its status.
fn reap_pinned(pid: i32, fd: &std::os::fd::OwnedFd) -> Option<i32> {
    let status = wait_pidfd(fd, rustix::process::WaitIdOptions::EXITED).map(|s| raw_status(&s));
    if let Some(status) = status {
        REAPED.lock().unwrap().insert(pid, status);
    }
    status
}

/// Kills and reaps one pinned direct child.
fn kill_pinned(pid: i32, fd: &std::os::fd::OwnedFd) {
    let _ = rustix::process::pidfd_send_signal(fd, rustix::process::Signal::KILL);
    reap_pinned(pid, fd);
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
        // One critical section per round: list, pin, signal, reap.
        let guard = CHILDREN.lock().unwrap();
        let children = other_children(leader);
        if children.is_empty() {
            return killed;
        }
        for child in children {
            if let Some(fd) = pin_child(child.pid) {
                kill_pinned(child.pid, &fd);
            }
            killed.push(child);
        }
        drop(guard);
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
                    // process that never exits. The thread pins each child under
                    // the lock, waits for its exit WITHOUT reaping, and reaps
                    // under the lock again; a child that a KILL reaped in
                    // between is simply gone.
                    std::thread::spawn(move || {
                        let mut joined = Vec::new();
                        let mut announced = false;
                        loop {
                            let pinned: Vec<(i32, std::os::fd::OwnedFd)> = {
                                let _guard = CHILDREN.lock().unwrap();
                                other_children(leader)
                                    .into_iter()
                                    .filter_map(|c| pin_child(c.pid).map(|fd| (c.pid, fd)))
                                    .collect()
                            };
                            if !announced {
                                // The first round has pinned its children: from
                                // here a KILL meets a JOIN that is really pending.
                                emit("JOINING", serde_json::json!(pinned.len()));
                                announced = true;
                            }
                            if pinned.is_empty() {
                                break;
                            }
                            for (pid, fd) in pinned {
                                let exit = wait_pidfd(
                                    &fd,
                                    rustix::process::WaitIdOptions::EXITED
                                        | rustix::process::WaitIdOptions::NOWAIT,
                                );
                                let _guard = CHILDREN.lock().unwrap();
                                // The status is the one seen while waiting, the one
                                // of this reap, or the one a KILL recorded when it
                                // reaped the child first. A child with none of the
                                // three is reported with the status unknown (-1).
                                let reaped = reap_pinned(pid, &fd);
                                let recorded = REAPED.lock().unwrap().remove(&pid);
                                let status = exit.as_ref().map(raw_status).or(reaped).or(recorded);
                                joined.push((pid, status.unwrap_or(-1)));
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
