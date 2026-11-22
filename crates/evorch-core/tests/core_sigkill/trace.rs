//! Independent syscall evidence; daemonized strace never kills its tracees.
use super::product::check;
use std::{fs, io, os::unix::ffi::OsStrExt, path::Path};
use tokio::process::Command;
pub fn command(program: &Path, trace: Option<&Path>) -> Command {
    if let Some(trace) = trace {
        let mut command = Command::new("strace");
        command
            .args([
                "-D",
                "-ff",
                "-qq",
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
pub fn assert_no_process_spawn(prefix: &Path, pid: i32) -> io::Result<()> {
    let path = prefix.with_extension(pid.to_string());
    let text = fs::read_to_string(&path)?;
    let mut entered = false;
    for line in text.lines() {
        if line.contains("execve(") || line.contains("execveat(") {
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
        if entered
            && (line.starts_with("clone(")
                || line.starts_with("clone3(")
                || line.starts_with("fork(")
                || line.starts_with("vfork("))
            && !line.contains("CLONE_THREAD")
            && !line.contains("= -1 ")
        {
            return Err(io::Error::other(format!(
                "worker created process after initial exec: {line}"
            )));
        }
    }
    check(
        entered,
        format!(
            "no independent initial js-worker exec in {}",
            path.display()
        ),
    )
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
        assert_no_process_spawn(prefix, worker)?;
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
