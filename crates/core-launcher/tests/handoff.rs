#![cfg(all(target_os = "linux", feature = "handoff-test-support"))]
use core_instance::{ConfigIdentity, LaunchGuard};
use core_launcher::{
    CoreBinary,
    handoff::{HandoffTarget, ProbeLimits},
};
use std::{
    fs, io,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
};
fn fixture() -> PathBuf {
    std::env::var_os("CARGO_BIN_EXE_core-handoff-fixture")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let test = std::env::current_exe().unwrap();
            test.parent()
                .unwrap()
                .parent()
                .unwrap()
                .join("core-handoff-fixture")
        })
}
fn quote(path: &Path) -> String {
    format!("'{}'", path.to_str().unwrap().replace('\'', "'\\''"))
}
fn wrapper(dir: &Path, extra: &str, pause: bool) -> PathBuf {
    let path = dir.join("wrapper");
    // makeWrapper-equivalent: add PATH, then exec the final ELF.
    let wait = if pause {
        "if [ \"$1\" != --describe-handoff ]; then printf 'wrapper\\n'; IFS= read -r token; fi\n"
    } else {
        ""
    };
    let bash = std::process::Command::new("bash")
        .args(["-c", "printf '%s' \"$BASH\""])
        .output()
        .unwrap();
    assert!(bash.status.success());
    let bash = String::from_utf8(bash.stdout).unwrap();
    fs::write(
        &path,
        format!(
            "#!{bash}\nexport PATH=/fixture/bin:\"$PATH\"\n{wait}exec -a \"$0\" {} {extra} \"$@\"\n",
            quote(&fixture())
        ),
    )
    .unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    path
}

// Prevent paused time's automatic advancement while a real OS child works.
// This is event-driven, not a completion timeout; nextest owns hang detection.
struct ClockHold {
    tx: Option<std::sync::mpsc::Sender<()>>,
    task: tokio::task::JoinHandle<()>,
}
impl ClockHold {
    async fn new() -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        let (ready, wait) = tokio::sync::oneshot::channel();
        let task = tokio::task::spawn_blocking(move || {
            ready.send(()).unwrap();
            let _ = rx.recv();
        });
        wait.await.unwrap();
        Self { tx: Some(tx), task }
    }
    async fn finish(mut self) {
        self.tx.take().unwrap().send(()).unwrap();
        self.task.await.unwrap();
    }
}

#[tokio::test(start_paused = true)]
async fn probes_same_wrapper_and_rejects_invalid_limits_output_build_and_symlink() {
    let clock = ClockHold::new().await;
    let dir = tempfile::tempdir().unwrap();
    let path = wrapper(dir.path(), "", false);
    let target = HandoffTarget::probe(&path, ProbeLimits::default())
        .await
        .unwrap();
    let d = target.description();
    assert_eq!(d.tool_catalog, "fixture-catalog-v1");
    assert_eq!(d.elf_path, fixture());
    d.require(1, 1, 62, 1, &format!("linux-{}", std::env::consts::ARCH))
        .unwrap();
    target.reprobe(ProbeLimits::default()).await.unwrap();
    for behavior in ["invalid", "large", "wrong-build"] {
        let p = wrapper(dir.path(), &format!("--probe-behavior {behavior}"), false);
        assert!(
            HandoffTarget::probe(&p, ProbeLimits::default())
                .await
                .is_err()
        );
    }
    let p = wrapper(dir.path(), "--probe-behavior old", false);
    let old = HandoffTarget::probe(&p, ProbeLimits::default())
        .await
        .unwrap();
    assert!(
        old.description()
            .require(1, 1, 62, 1, &old.description().platform)
            .is_err()
    );
    let link = dir.path().join("link");
    std::os::unix::fs::symlink(&p, &link).unwrap();
    assert!(CoreBinary::explicit(&link).is_err());
    assert!(
        HandoffTarget::probe(&link, ProbeLimits::default())
            .await
            .is_err()
    );
    for limits in [
        ProbeLimits {
            timeout: Duration::ZERO,
            output_bytes: 1,
        },
        ProbeLimits {
            timeout: Duration::from_secs(61),
            output_bytes: 1,
        },
        ProbeLimits {
            timeout: Duration::from_secs(1),
            output_bytes: 0,
        },
        ProbeLimits {
            timeout: Duration::from_secs(1),
            output_bytes: 65537,
        },
    ] {
        assert_eq!(
            HandoffTarget::probe(&p, limits).await.err().unwrap().kind(),
            io::ErrorKind::InvalidInput
        );
    }
    assert!(target.revalidate().is_err()); // entrypoint overwritten after probe
    clock.finish().await;
}

async fn interrupted_probe(behavior: &str, timeout: bool) {
    let clock = ClockHold::new().await;
    let dir = test_socket_dir::SocketDir::new();
    let socket = dir.path().join("n");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let dist = tempfile::tempdir().unwrap();
    let path = wrapper(
        dist.path(),
        &format!("--probe-behavior {behavior} --notify {}", quote(&socket)),
        false,
    );
    let (tx, rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        HandoffTarget::probe_cancellable(
            &path,
            ProbeLimits {
                timeout: Duration::from_secs(5),
                output_bytes: 4096,
            },
            async {
                let _ = rx.await;
            },
        )
        .await
    });
    let (mut stream, _) = listener.accept().await.unwrap();
    let mut pids = String::new();
    stream.read_to_string(&mut pids).await.unwrap();
    if timeout {
        tokio::time::advance(Duration::from_secs(5)).await;
    } else {
        tx.send(()).unwrap();
    }
    let error = task.await.unwrap().err().unwrap();
    assert_eq!(
        error.kind(),
        if timeout {
            io::ErrorKind::TimedOut
        } else {
            io::ErrorKind::Interrupted
        }
    );
    for pid in pids.split_whitespace() {
        assert!(
            !Path::new(&format!("/proc/{pid}")).exists(),
            "unreaped descendant {pid}"
        );
    }
    clock.finish().await;
}
#[tokio::test(start_paused = true)]
async fn timeout_reaps_group_and_grandchildren() {
    interrupted_probe("hang", true).await;
}
#[tokio::test(start_paused = true)]
async fn cancellation_reaps_escaped_session_and_its_child() {
    interrupted_probe("escape", false).await;
}
#[tokio::test(start_paused = true)]
async fn timeout_reaps_escaped_session_and_its_child() {
    interrupted_probe("escape", true).await;
}

struct Process {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
}
impl Process {
    fn start(config: &Path, ipc: &Path, target: &Path, scenario: &str) -> Self {
        let mut child = Command::new(fixture())
            .args(["--scenario", scenario, "--config"])
            .arg(config)
            .arg("--ipc-parent")
            .arg(ipc)
            .arg("--target")
            .arg(target)
            .current_dir(config)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let input = child.stdin.take().unwrap();
        let output = BufReader::new(child.stdout.take().unwrap());
        Self {
            child,
            input,
            output,
        }
    }
    async fn line(&mut self) -> String {
        let mut s = String::new();
        assert!(
            self.output.read_line(&mut s).await.unwrap() > 0,
            "fixture exited: {:?}",
            self.child.try_wait()
        );
        s.trim().into()
    }
    async fn go(&mut self) {
        self.input
            .write_all(
                b"x
",
            )
            .await
            .unwrap();
    }
    async fn finish(mut self) {
        assert!(self.child.wait().await.unwrap().success());
    }
}
#[tokio::test]
async fn multithread_shell_wrapper_exec_preserves_lock_fd_allowlist_signals_cwd_env_and_pid() {
    let config = tempfile::tempdir().unwrap();
    let ipc = test_socket_dir::SocketDir::new();
    let dist = tempfile::tempdir().unwrap();
    let path = wrapper(dist.path(), "", true);
    let mut p = Process::start(config.path(), ipc.path(), &path, "multithread");
    let pid = p.child.id().unwrap();
    assert_eq!(p.line().await, "before-exec");
    let identity = ConfigIdentity::new(config.path()).unwrap();
    let launch = LaunchGuard::acquire(&identity).unwrap();
    assert!(!launch.lifetime_available().unwrap());
    // Queue both signals while the predecessor still has multiple threads.
    // They must cross raw execve AND the bash wrapper's exec as pending signals.
    assert_eq!(unsafe { libc::kill(pid as i32, libc::SIGINT) }, 0);
    assert_eq!(unsafe { libc::kill(pid as i32, libc::SIGTERM) }, 0);
    p.go().await;
    assert_eq!(p.line().await, "wrapper");
    let status = fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
    let pending = u64::from_str_radix(
        status
            .lines()
            .find_map(|l| l.strip_prefix("ShdPnd:"))
            .unwrap()
            .trim(),
        16,
    )
    .unwrap();
    let both = (1 << (libc::SIGINT - 1)) | (1 << (libc::SIGTERM - 1));
    assert_eq!(pending & both, both);
    p.input.write_all(b"continue\n").await.unwrap();
    let line = p.line().await;
    assert!(
        line.starts_with(&format!("adopted {pid} 1 signals=3 path=/fixture/bin:")),
        "{line}"
    );
    assert!(!launch.lifetime_available().unwrap());
    p.go().await;
    p.finish().await;
    assert!(launch.lifetime_available().unwrap());
}
#[tokio::test]
async fn repeated_wrapper_exec_uses_new_ticket_without_stale_launch_arguments() {
    let config = tempfile::tempdir().unwrap();
    let ipc = test_socket_dir::SocketDir::new();
    let dist = tempfile::tempdir().unwrap();
    let path = wrapper(dist.path(), "", false);
    let mut p = Process::start(config.path(), ipc.path(), &path, "twice");
    let pid = p.child.id().unwrap();
    assert_eq!(p.line().await, "before-exec");
    p.go().await;
    assert!(p.line().await.starts_with(&format!("adopted {pid} 1 ")));
    assert_eq!(p.line().await, "before-exec");
    p.go().await;
    let line = p.line().await;
    assert!(line.starts_with(&format!("adopted {pid} 2 ")));
    assert!(line.contains("path=/fixture/bin:/fixture/bin:"));
    p.go().await;
    p.finish().await;
}
#[tokio::test]
async fn adoption_failures_are_fatal_and_callback_is_once_only() {
    for (scenario, message) in [
        ("leak", "rejected-fds"),
        ("mismatch", "rejected-adoption"),
        ("callback-fails", "rejected-adoption"),
    ] {
        let config = tempfile::tempdir().unwrap();
        let ipc = test_socket_dir::SocketDir::new();
        let mut p = Process::start(config.path(), ipc.path(), &fixture(), scenario);
        assert_eq!(p.line().await, "before-exec");
        p.go().await;
        assert_eq!(p.line().await, message);
        p.finish().await;
    }
}
#[tokio::test]
async fn failed_exec_restores_attributes_and_keeps_lifetime_lock() {
    let config = tempfile::tempdir().unwrap();
    let ipc = test_socket_dir::SocketDir::new();
    let mut p = Process::start(config.path(), ipc.path(), &fixture(), "exec-failure");
    assert_eq!(p.line().await, "exec-failed-restored");
    let identity = ConfigIdentity::new(config.path()).unwrap();
    let launch = LaunchGuard::acquire(&identity).unwrap();
    assert!(!launch.lifetime_available().unwrap());
    p.go().await;
    p.finish().await;
    assert!(launch.lifetime_available().unwrap());
}
#[tokio::test]
async fn final_fence_rejects_worker_that_unblocks_shutdown_signals() {
    let config = tempfile::tempdir().unwrap();
    let ipc = test_socket_dir::SocketDir::new();
    let mut p = Process::start(config.path(), ipc.path(), &fixture(), "thread-fence");
    assert_eq!(p.line().await, "thread-fence-rejected");
    p.finish().await;
}

#[tokio::test]
async fn parent_death_stops_supervisor_and_reaps_escaped_descendants() {
    let dir = test_socket_dir::SocketDir::new();
    let socket = dir.path().join("n");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let dist = tempfile::tempdir().unwrap();
    let path = wrapper(
        dist.path(),
        &format!("--probe-behavior escape --notify {}", quote(&socket)),
        false,
    );
    let mut child = Command::new(fixture())
        .arg("--probe-parent")
        .arg("--target")
        .arg(path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let (mut stream, _) = listener.accept().await.unwrap();
    let mut pids = String::new();
    stream.read_to_string(&mut pids).await.unwrap();
    let supervisor = pids.split_whitespace().next().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(format!("{supervisor}\n").as_bytes())
        .await
        .unwrap();
    let output = child.wait_with_output().await.unwrap();
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().trim(),
        "parent-death-cleaned"
    );
    for pid in pids.split_whitespace() {
        assert!(
            !Path::new(&format!("/proc/{pid}")).exists(),
            "unreaped descendant {pid}"
        );
    }
}
