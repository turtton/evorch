//! Independent syscall evidence; daemonized strace never kills its tracees.
use super::product::check;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
    os::unix::ffi::OsStrExt,
    path::Path,
};
use tokio::process::Command;
pub fn command(program: &Path, trace: Option<&Path>) -> Command {
    if let Some(trace) = trace {
        let mut command = Command::new("strace");
        command
            .args([
                "-D",
                "-ff",
                "-qq",
                "--decode-pids=pidns",
                "-s",
                "256",
                "-e",
                "trace=process,kill",
                "-o",
            ])
            .arg(trace)
            .arg(program);
        command
    } else {
        Command::new(program)
    }
}
fn is_spawn_call(line: &str) -> bool {
    (line.starts_with("clone(")
        || line.starts_with("clone3(")
        || line.starts_with("fork(")
        || line.starts_with("vfork("))
        && !line.contains("CLONE_THREAD")
}
fn is_exec_call(line: &str) -> bool {
    line.contains("execve(") || line.contains("execveat(")
}
fn call_result(line: &str) -> Option<i32> {
    let result = line.rsplit_once(" = ")?.1;
    // `--decode-pids=pidns` adds the host-side id when the tracee runs in
    // another PID namespace (bwrap): `= 5 /* 123 in strace's PID NS */`.
    if let Some((_, rest)) = result.split_once("/*") {
        return rest.split_whitespace().next()?.parse().ok();
    }
    result.split_whitespace().next()?.parse().ok()
}
/// Every `strace -ff` log of the prefix, keyed by TID (threads and exited
/// threads each own a file).
fn read_logs(prefix: &Path) -> io::Result<BTreeMap<i32, String>> {
    let dir = prefix.parent().unwrap_or_else(|| Path::new("."));
    let stem = prefix
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| io::Error::other("trace prefix is not UTF-8"))?;
    let mut logs = BTreeMap::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(tid) = name
            .to_str()
            .and_then(|name| name.strip_prefix(stem))
            .and_then(|rest| rest.strip_prefix('.'))
            .and_then(|tid| tid.parse::<i32>().ok())
        else {
            continue;
        };
        logs.insert(
            tid,
            String::from_utf8_lossy(&fs::read(entry.path())?).into_owned(),
        );
    }
    Ok(logs)
}
/// `complete` is false for a mid-run look (a thread log may not exist yet);
/// the closing check after the tracer exited passes true.
pub fn assert_no_process_spawn(prefix: &Path, pid: i32, complete: bool) -> io::Result<()> {
    inspect(&read_logs(prefix)?, pid, complete)
}
fn inspect(logs: &BTreeMap<i32, String>, leader: i32, complete: bool) -> io::Result<()> {
    let text = logs
        .get(&leader)
        .ok_or_else(|| io::Error::other(format!("no strace log for worker {leader}")))?;
    let mut entered = false;
    let mut threads = Vec::new();
    for line in text.lines() {
        if is_exec_call(line) {
            if !entered && line.contains("js-worker") && line.ends_with("= 0") {
                entered = true;
                continue;
            }
            if entered {
                return Err(io::Error::other(format!(
                    "worker attempted exec after initial exec: {line}"
                )));
            }
        }
        if entered && line.contains("CLONE_THREAD") {
            threads.extend(call_result(line).filter(|tid| *tid > 0));
        }
        if entered && is_spawn_call(line) && call_result(line) != Some(-1) {
            return Err(io::Error::other(format!(
                "worker created process after initial exec: {line}"
            )));
        }
    }
    check(
        entered,
        format!("no independent initial js-worker exec for worker {leader}"),
    )?;
    // Threads exist only after the initial exec, so none of their lines is
    // exempt. A thread may create more threads; follow them all.
    let mut seen = BTreeSet::from([leader]);
    while let Some(tid) = threads.pop() {
        if !seen.insert(tid) {
            continue;
        }
        let Some(text) = logs.get(&tid) else {
            check(!complete, format!("no strace log for worker thread {tid}"))?;
            continue;
        };
        for line in text.lines() {
            if is_exec_call(line) && line.ends_with("= 0") {
                return Err(io::Error::other(format!(
                    "worker thread {tid} exec'd: {line}"
                )));
            }
            if line.contains("CLONE_THREAD") {
                threads.extend(call_result(line).filter(|tid| *tid > 0));
            }
            if is_spawn_call(line) && call_result(line) != Some(-1) {
                return Err(io::Error::other(format!(
                    "worker thread {tid} created process: {line}"
                )));
            }
        }
    }
    Ok(())
}

pub fn tracer() -> io::Result<super::observer::PinnedProcess> {
    let owner = super::observer::PinnedProcess::capture(std::process::id() as i32)?;
    let mut found = Vec::new();
    for value in fs::read_to_string(
        "/proc/self/task/".to_owned() + &std::process::id().to_string() + "/children",
    )?
    .split_whitespace()
    {
        let Ok(pid) = value.parse::<i32>() else {
            continue;
        };
        let Ok(identity) = super::observer::Identity::read(pid) else {
            continue;
        };
        let Ok(pin) = super::observer::PinnedProcess::pin(&identity) else {
            continue;
        };
        if pin.identity.parent != owner.identity.pid {
            continue;
        }
        let Ok(args) = fs::read(format!("/proc/{pid}/cmdline")) else {
            continue;
        };
        if args.split(|b| *b == 0).next().is_some_and(|arg| {
            Path::new(std::ffi::OsStr::from_bytes(arg)).file_name()
                == Some(std::ffi::OsStr::new("strace"))
        }) && pin.unchanged()
        {
            found.push(pin);
        }
    }
    check(
        owner.unchanged() && found.len() == 1,
        "independent strace daemon missing or ambiguous",
    )?;
    Ok(found.remove(0))
}
pub async fn finish(
    cell: &str,
    prefix: &Path,
    worker: Option<i32>,
    tracer: &super::observer::PinnedProcess,
    guard: &serde_json::Value,
) -> io::Result<()> {
    super::observer::bounded(
        std::time::Duration::from_secs(30),
        "strace closure",
        tracer.exited(),
    )
    .await?;
    if let Some(worker) = worker {
        assert_no_process_spawn(prefix, worker, true)?;
        let trace = fs::read_to_string(prefix.with_extension(worker.to_string()))?;
        check(
            trace.contains("+++ killed by SIGKILL +++"),
            "worker trace has no terminal SIGKILL",
        )?;
        super::evidence::emit(
            cell,
            "no-spawn",
            &serde_json::json!({"worker_pid":worker,"tracer":tracer.identity,"trace":trace}),
        )?;
    }
    let helper = guard["identity"]["pid"]
        .as_i64()
        .ok_or_else(|| io::Error::other("guard PID missing"))?;
    let root = guard["target_root"]
        .as_i64()
        .ok_or_else(|| io::Error::other("guard root missing"))?;
    let trace = fs::read_to_string(prefix.with_extension(helper.to_string()))?;
    let lines: Vec<_> = trace.lines().collect();
    let reap = lines
        .iter()
        .position(|line| line.starts_with(&format!("wait4({root},")))
        .ok_or_else(|| io::Error::other("guard has no final root reap"))?;
    let signal = format!("kill(-{root}, SIGKILL)");
    check(
        lines[..reap].iter().any(|line| line.starts_with(&signal)),
        "root was reaped before group KILL",
    )?;
    check(
        !lines[reap..]
            .iter()
            .any(|line| line.starts_with(&format!("kill(-{root},"))),
        "group signalled after root reap",
    )?;
    super::evidence::emit(
        cell,
        "reap-order",
        &serde_json::json!({"helper_pid":helper,"root_pid":root,"trace":trace}),
    )
}

/// Fixture body (`--thread-fixture <spawn|thread> js-worker`): only a thread
/// ever creates something. The `js-worker` argument makes the traced initial
/// exec look like the worker's.
pub fn thread_fixture(mode: &str) -> io::Result<()> {
    let mode = mode.to_owned();
    std::thread::spawn(move || {
        if mode == "spawn" {
            let _ = std::process::Command::new("/bin/true").status();
        }
    })
    .join()
    .map_err(|_| io::Error::other("fixture thread panicked"))
}

/// Negative control for the L08 no-spawn check: a real strace of a process
/// whose only *thread* forks a short-lived child must be rejected, while a
/// thread-only process is accepted.
pub async fn thread_spawn_control(own: &Path) -> io::Result<()> {
    use super::{observer::bounded, product::EXIT};
    for (mode, rejected) in [("thread", false), ("spawn", true)] {
        let prefix = own.with_file_name(format!("thread-trace-{mode}"));
        let mut command = command(own, Some(&prefix));
        command.args(["--thread-fixture", mode, "js-worker"]);
        bounded(EXIT, "strace thread fixture start", async {
            command.status().await.and_then(|status| {
                check(status.success(), format!("strace {mode} fixture: {status}"))
            })
        })
        .await?;
        // -D detaches the tracer and -qq drops exit notices: wait for exit_group.
        let logs = bounded(EXIT, "strace thread fixture closure", async {
            loop {
                let logs = read_logs(&prefix)?;
                let leader = logs.iter().find(|(_, text)| {
                    text.lines()
                        .any(|l| is_exec_call(l) && l.contains("js-worker"))
                });
                if leader.is_some_and(|(_, text)| text.contains("exit_group(")) {
                    return Ok(logs);
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await?;
        let leader = *logs
            .iter()
            .find(|(_, text)| {
                text.lines()
                    .any(|l| is_exec_call(l) && l.contains("js-worker"))
            })
            .map(|(tid, _)| tid)
            .unwrap();
        check(
            logs.len() > 1,
            format!("{mode}: no separate strace log for the fixture thread"),
        )?;
        match (inspect(&logs, leader, true), rejected) {
            (Ok(()), false) => {}
            (Err(error), true) => check(
                error.to_string().contains("worker thread"),
                format!("rejected for the wrong reason: {error}"),
            )?,
            (Ok(()), true) => {
                return Err(io::Error::other(
                    "a process created from another thread was not detected",
                ));
            }
            (Err(error), false) => return Err(error),
        }
    }
    Ok(())
}
