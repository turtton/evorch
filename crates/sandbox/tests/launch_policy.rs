//! The launch policy of `sandbox::process`, which every child evorch starts
//! outside a pseudo-terminal gets.
//!
//! A child must not reach evorch's controlling terminal (reading it from a
//! background process group stops the child with SIGTTIN for good), must not
//! inherit descriptors evorch holds without `O_CLOEXEC` (the GUI's browser
//! engine leaves many), and must stop together with its descendants.

use std::io::{BufRead, BufReader, Read};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use rustix::process::Pid;
use sandbox::process::{
    ProcessGroupGuard, apply_launch_policy, close_inherited_fds_on_exec, detach_from_terminal,
};
use sandbox::{CommandSpec, DirectSandbox, Sandbox, StdioSession};

const CHILD_FLAG: &str = "SANDBOX_LAUNCH_POLICY_TERMINAL_CHILD";
const TERMINAL_TEST: &str = "children_cannot_reach_the_evorch_terminal";
/// Opening `/dev/tty` only succeeds while the child has a controlling
/// terminal; unlike reading it, opening never stops the child.
const TERMINAL_PROBE: &str = "if (exec 3</dev/tty) 2>/dev/null; then echo terminal-reachable; else echo terminal-detached; fi";

fn spec(program: &str, args: &[&str]) -> CommandSpec {
    CommandSpec {
        program: program.to_owned(),
        args: args.iter().map(|arg| (*arg).to_owned()).collect(),
        cwd: None,
        extra_env: Vec::new(),
    }
}

fn sh(script: &str) -> Command {
    let mut command = Command::new("sh");
    command.args(["-c", script]);
    command
}

fn stdout_of(mut command: Command) -> String {
    let output = command
        .stdin(Stdio::null())
        .stderr(Stdio::inherit())
        .output()
        .expect("run the probe");
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn with(mut command: Command, policy: fn(&mut Command)) -> Command {
    policy(&mut command);
    command
}

/// Wait until `pid` no longer runs; a zombie waiting for its reaper counts as stopped.
fn assert_stops(pid: Pid) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"));
        let running = stat.is_ok_and(|stat| {
            stat.rsplit_once(')')
                .is_some_and(|(_, rest)| !rest.trim_start().starts_with('Z'))
        });
        if !running {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "process {pid} still runs after its group was killed"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn read_pid(reader: impl Read) -> Pid {
    let mut line = String::new();
    BufReader::new(reader)
        .read_line(&mut line)
        .expect("read the reported pid");
    Pid::from_raw(line.trim().parse().expect("a pid")).expect("a positive pid")
}

// Given: evorch runs with a controlling terminal (the test re-runs itself
// inside a pseudo-terminal) / When: children start with the launch policy, as
// std and Tokio commands, wrapped commands and stdio sessions / Then: none of
// them can open that terminal, while a child without the policy can.
#[test]
fn children_cannot_reach_the_evorch_terminal() {
    if std::env::var_os(CHILD_FLAG).is_some() {
        run_terminal_probes();
        return;
    }
    let pair = native_pty_system()
        .openpty(PtySize::default())
        .expect("open a pseudo-terminal");
    let mut child_test =
        CommandBuilder::new(std::env::current_exe().expect("locate the test executable"));
    child_test.args(["--exact", TERMINAL_TEST, "--nocapture"]);
    child_test.env(CHILD_FLAG, "1");
    child_test.cwd(std::env::current_dir().expect("current directory"));
    let mut child = pair
        .slave
        .spawn_command(child_test)
        .expect("start the test inside the pseudo-terminal");
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader().expect("terminal reader");
    let mut transcript = Vec::new();
    let mut buffer = [0u8; 4096];
    // Linux reports EIO on the master once the last slave descriptor closes.
    while let Ok(n) = reader.read(&mut buffer) {
        if n == 0 {
            break;
        }
        transcript.extend_from_slice(&buffer[..n]);
    }
    let status = child.wait().expect("wait for the child test");
    let transcript = String::from_utf8_lossy(&transcript);
    assert!(status.success(), "child test failed:\n{transcript}");
}

fn run_terminal_probes() {
    assert!(
        std::fs::File::open("/dev/tty").is_ok(),
        "the child test itself must own a controlling terminal"
    );
    assert_eq!(
        stdout_of(sh(TERMINAL_PROBE)),
        "terminal-reachable\n",
        "without the policy a child shares the terminal, or this probe proves nothing"
    );
    assert_eq!(
        stdout_of(with(sh(TERMINAL_PROBE), detach_from_terminal)),
        "terminal-detached\n"
    );
    assert_eq!(
        stdout_of(with(sh(TERMINAL_PROBE), apply_launch_policy)),
        "terminal-detached\n"
    );
    let wrapped = DirectSandbox::new_unchecked()
        .wrap(spec("sh", &["-c", TERMINAL_PROBE]))
        .expect("wrap the probe");
    assert_eq!(stdout_of(wrapped.to_command()), "terminal-detached\n");

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build the test runtime");
    let mut tokio_command = tokio::process::Command::new("sh");
    tokio_command
        .args(["-c", TERMINAL_PROBE])
        .stdin(Stdio::null());
    apply_launch_policy(tokio_command.as_std_mut());
    let output = runtime
        .block_on(async { tokio_command.output().await })
        .expect("run the Tokio probe");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "terminal-detached\n"
    );

    let mut session = StdioSession::spawn(
        &DirectSandbox::new_unchecked(),
        spec("sh", &["-c", TERMINAL_PROBE]),
    )
    .expect("start the stdio session");
    let mut stdout = String::new();
    session
        .take_stdout()
        .expect("session stdout")
        .read_to_string(&mut stdout)
        .expect("read the session probe");
    assert_eq!(stdout, "terminal-detached\n");
}

/// A descriptor numbered 100 or above that a child would inherit (no
/// `O_CLOEXEC`), referring to a temporary file with a unique name.
fn inheritable_descriptor() -> (tempfile::NamedTempFile, OwnedFd, String) {
    let file = tempfile::NamedTempFile::new().expect("temporary file");
    let fd = rustix::io::fcntl_dupfd_cloexec(&file, 100).expect("duplicate the descriptor");
    rustix::io::fcntl_setfd(&fd, rustix::io::FdFlags::empty()).expect("clear FD_CLOEXEC");
    let name = file
        .path()
        .file_name()
        .and_then(|name| name.to_str())
        .expect("UTF-8 file name")
        .to_owned();
    (file, fd, name)
}

// Given: evorch holds a descriptor without O_CLOEXEC / When: children start
// with the policy as std, Tokio and wrapped commands / Then: none of them sees
// that descriptor number, while a child without the policy sees the file.
#[tokio::test]
async fn children_do_not_inherit_descriptors_beyond_their_standard_streams() {
    let (_file, fd, name) = inheritable_descriptor();
    let probe = format!("readlink /proc/self/fd/{} || true", fd.as_raw_fd());
    assert!(
        stdout_of(sh(&probe)).contains(&name),
        "without the policy the child inherits the descriptor, or this probe proves nothing"
    );
    for (label, command) in [
        (
            "close_inherited_fds_on_exec",
            with(sh(&probe), close_inherited_fds_on_exec),
        ),
        ("apply_launch_policy", with(sh(&probe), apply_launch_policy)),
        (
            "WrappedCommand::to_command",
            DirectSandbox::new_unchecked()
                .wrap(spec("sh", &["-c", &probe]))
                .expect("wrap the probe")
                .to_command(),
        ),
    ] {
        let seen = stdout_of(command);
        assert!(
            !seen.contains(&name),
            "{label} leaked the descriptor: {seen}"
        );
    }
    let mut tokio_command = tokio::process::Command::new("sh");
    tokio_command.args(["-c", &probe]).stdin(Stdio::null());
    apply_launch_policy(tokio_command.as_std_mut());
    let output = tokio_command.output().await.expect("run the Tokio probe");
    let seen = String::from_utf8_lossy(&output.stdout);
    assert!(
        !seen.contains(&name),
        "Tokio command leaked the descriptor: {seen}"
    );
}

// Given: a program that does not exist / When: it starts with the policy /
// Then: the spawn still reports NotFound, through the exec error pipe.
#[tokio::test]
async fn a_missing_program_is_reported_as_not_found() {
    let absent = "/nonexistent/evorch-launch-policy-probe";
    let error = with(Command::new(absent), apply_launch_policy)
        .spawn()
        .expect_err("the program is missing");
    assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
    let mut tokio_command = tokio::process::Command::new("evorch-launch-policy-probe-on-no-path");
    apply_launch_policy(tokio_command.as_std_mut());
    let error = tokio_command.spawn().expect_err("the program is missing");
    assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
    let wrapped = DirectSandbox::new_unchecked()
        .wrap(spec(absent, &[]))
        .expect("wrap the command");
    let error = wrapped
        .to_command()
        .spawn()
        .expect_err("the program is missing");
    assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
}

// Given: a detached child that started a background grandchild / When: its
// group guard is dropped / Then: the child leads its own session and group,
// the grandchild stays in that group, and both stop.
#[test]
fn dropping_the_group_guard_stops_descendants() {
    let mut command = with(sh("sleep 60 & echo $!; wait"), apply_launch_policy);
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .expect("start the child");
    let group = ProcessGroupGuard::new(Some(child.id()));
    let leader = group.leader().expect("the child's pid");
    let grandchild = read_pid(child.stdout.take().expect("child stdout"));
    assert_eq!(rustix::process::getsid(Some(leader)), Ok(leader));
    assert_eq!(rustix::process::getpgid(Some(leader)), Ok(leader));
    assert_eq!(rustix::process::getpgid(Some(grandchild)), Ok(leader));
    drop(group);
    child.wait().expect("reap the child");
    assert_stops(grandchild);
}

// Given: a stdio server that started a background helper / When: the session
// is dropped / Then: the helper stops with the server.
#[test]
fn dropping_a_stdio_session_stops_its_descendants() {
    let mut session = StdioSession::spawn(
        &DirectSandbox::new_unchecked(),
        spec("sh", &["-c", "sleep 60 & echo $!; exec cat"]),
    )
    .expect("start the stdio session");
    let helper = read_pid(session.take_stdout().expect("session stdout"));
    drop(session);
    assert_stops(helper);
}

// Given: a command that already asks for process_group(0) / When: it is also
// detached / Then: the spawn fails with EPERM, as the documentation warns;
// the standard library makes the child a group leader before the hook runs.
#[test]
fn detaching_a_process_group_leader_fails() {
    let mut command = Command::new("true");
    command.process_group(0);
    detach_from_terminal(&mut command);
    let error = command
        .spawn()
        .expect_err("setsid fails for a group leader");
    assert_eq!(
        error.raw_os_error(),
        Some(rustix::io::Errno::PERM.raw_os_error())
    );
}
