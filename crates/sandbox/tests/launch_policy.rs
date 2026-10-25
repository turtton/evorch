//! The launch policy of `sandbox::process`, which every child evorch starts
//! outside a pseudo-terminal gets.
//!
//! A child must not reach evorch's controlling terminal (reading it from a
//! background process group stops the child with SIGTTIN for good), must not
//! inherit descriptors evorch holds without `O_CLOEXEC` (the GUI's browser
//! engine leaves many), and must stop together with its descendants without a
//! reaped group id ever being signalled.

use std::io::Read;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use sandbox::process::{apply_launch_policy, detach_from_terminal};
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

/// Expectations that need `/proc`, `close_range` or a pidfd.
#[cfg(target_os = "linux")]
mod linux {
    use std::io::{BufRead, BufReader};
    use std::os::fd::{AsRawFd, OwnedFd};
    use std::time::{Duration, Instant};

    use rustix::process::Pid;
    use sandbox::process::{
        GroupChild, ProcessGroup, ProcessGroupGuard, apply_launch_policy,
        close_inherited_fds_on_exec, has_exited,
    };
    use sandbox::{BwrapConfig, BwrapSandbox, DirectSandbox, Sandbox, StdioSession};
    use tokio::io::{AsyncBufReadExt, AsyncReadExt};

    use super::{sh, spec, stdout_of, with};

    fn running(pid: Pid) -> bool {
        std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
            stat.rsplit_once(')')
                .is_some_and(|(_, rest)| !rest.trim_start().starts_with('Z'))
        })
    }

    fn zombie(pid: Pid) -> bool {
        std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
            stat.rsplit_once(')')
                .is_some_and(|(_, rest)| rest.trim_start().starts_with('Z'))
        })
    }

    /// Wait until `pid` no longer runs; a zombie waiting for its reaper counts as stopped.
    fn assert_stops(pid: Pid) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while running(pid) {
            assert!(
                Instant::now() < deadline,
                "process {pid} still runs after its group was killed"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn parse_pid(line: &str) -> Pid {
        Pid::from_raw(line.trim().parse().expect("a pid")).expect("a positive pid")
    }

    fn read_pid(reader: impl std::io::Read) -> Pid {
        let mut line = String::new();
        BufReader::new(reader)
            .read_line(&mut line)
            .expect("read the reported pid");
        parse_pid(&line)
    }

    /// A Tokio command running `script` under the launch policy.
    fn detached(script: &str) -> tokio::process::Command {
        let mut command = tokio::process::Command::from(with(sh(script), apply_launch_policy));
        command
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .kill_on_drop(true);
        command
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
        tokio_command
            .args(["-c", &probe])
            .stdin(std::process::Stdio::null());
        apply_launch_policy(tokio_command.as_std_mut());
        let output = tokio_command.output().await.expect("run the Tokio probe");
        let seen = String::from_utf8_lossy(&output.stdout);
        assert!(
            !seen.contains(&name),
            "Tokio command leaked the descriptor: {seen}"
        );
    }

    // Given: evorch holds a descriptor without O_CLOEXEC / When: a command runs
    // inside bubblewrap / Then: bubblewrap hands the descriptor on to a command
    // started without the policy, but the command from to_command sees neither it
    // nor evorch's session.
    #[ignore = "bwrap 実行環境が必要"]
    #[test]
    fn bwrap_commands_do_not_inherit_descriptors() {
        let workspace = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).expect("workspace");
        let sandbox = BwrapSandbox::detect(BwrapConfig::new(workspace.path().to_path_buf()))
            .expect("bwrap required");
        let (_file, fd, name) = inheritable_descriptor();
        let probe = format!(
            "cut -d' ' -f6 /proc/$$/stat; readlink /proc/$$/fd/{} || echo descriptor-closed",
            fd.as_raw_fd()
        );
        let wrapped = sandbox
            .wrap(spec("sh", &["-c", &probe]))
            .expect("wrap the probe");
        let mut plain = std::process::Command::new(&wrapped.program);
        wrapped.apply_environment(&mut plain);
        plain.args(&wrapped.args);
        let plain = stdout_of(plain);
        assert!(
            plain.contains(&name),
            "without the policy bubblewrap passes the descriptor on, or this probe proves nothing: {plain}"
        );
        let detached = stdout_of(wrapped.to_command());
        let lines: Vec<&str> = detached.lines().collect();
        let [session, descriptor] = lines[..] else {
            panic!("{detached}");
        };
        assert!(session.parse::<u32>().is_ok(), "{detached}");
        assert_ne!(
            session,
            rustix::process::getsid(None)
                .expect("own session")
                .to_string()
        );
        assert_eq!(descriptor, "descriptor-closed", "{detached}");
    }

    // Given: a detached child that started a background grandchild / When: a
    // guard of its group is dropped / Then: the child leads its own session and
    // group, the grandchild stays in that group, and both stop.
    #[test]
    fn dropping_a_group_guard_stops_descendants() {
        let mut command = with(sh("sleep 60 & echo $!; wait"), apply_launch_policy);
        let mut child = command
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("start the child");
        let group = ProcessGroup::new(Some(child.id()));
        let leader = group.leader().expect("the child's pid");
        let grandchild = read_pid(child.stdout.take().expect("child stdout"));
        assert_eq!(rustix::process::getsid(Some(leader)), Ok(leader));
        assert_eq!(rustix::process::getpgid(Some(leader)), Ok(leader));
        assert_eq!(rustix::process::getpgid(Some(grandchild)), Ok(leader));
        drop(ProcessGroupGuard::new(group));
        child.wait().expect("reap the child");
        assert_stops(grandchild);
    }

    // Given: a live detached child / When: a handle to its group is released
    // (or retired by a reap) and then asked to kill / Then: nothing is
    // signalled, so a reaped and reused id could not be reached either.
    #[test]
    fn a_retired_group_signals_nothing() {
        let mut command = with(sh("exec sleep 60"), apply_launch_policy);
        let mut child = command.spawn().expect("start the child");
        let group = ProcessGroup::new(Some(child.id()));
        let leader = group.leader().expect("the child's pid");
        let clone = group.clone();
        group.release();
        clone.kill();
        drop(ProcessGroupGuard::new(clone));
        std::thread::sleep(Duration::from_millis(200));
        assert!(running(leader), "a released group was signalled");
        assert!(!has_exited(leader).expect("an unreaped child"));

        let reaping = ProcessGroup::new(Some(child.id()));
        let status = reaping
            .kill_and_reap(|| child.wait().map(Some))
            .expect("reap the child")
            .expect("a status");
        assert!(!status.success());
        assert_eq!(reaping.leader(), None);
        assert!(child.try_wait().expect("reaped status").is_some());
    }

    // Given: a leader that exits while a background helper keeps its stdout
    // open / When: the child is awaited / Then: the exited leader stays
    // unreaped (a zombie reserving the group id) until the helper is killed,
    // the output then ends, and the reaped group is retired.
    #[tokio::test]
    async fn waiting_kills_the_helpers_of_an_exited_leader_before_reaping_it() {
        let mut child =
            GroupChild::spawn(&mut detached("sleep 60 & echo $!; exit 0")).expect("spawn");
        let leader = child.group().leader().expect("the leader's pid");
        let handle = child.group().clone();
        let mut stdout = tokio::io::BufReader::new(child.take_stdout().expect("stdout"));
        let mut line = String::new();
        stdout
            .read_line(&mut line)
            .await
            .expect("read the helper pid");
        let helper = parse_pid(&line);

        child.exited().await.expect("observe the exit");
        assert!(zombie(leader), "the exited leader must stay unreaped");
        assert!(running(helper), "nothing is killed before the wait");
        assert_eq!(handle.leader(), Some(leader));

        let status = tokio::time::timeout(Duration::from_secs(10), child.wait())
            .await
            .expect("waiting must not depend on the helper's pipe")
            .expect("reap the leader");
        assert!(status.success());
        assert_eq!(handle.leader(), None, "the reaped group is retired");
        assert_stops(helper);
        let mut rest = Vec::new();
        tokio::time::timeout(Duration::from_secs(10), stdout.read_to_end(&mut rest))
            .await
            .expect("the output ends once the helper is killed")
            .expect("read the rest");
        drop(child);
    }

    // Given: a running child / When: it is killed and reaped, then dropped
    // later / Then: the group is retired before the drop, for every clone.
    #[tokio::test]
    async fn kill_and_wait_retires_the_group_before_a_later_drop() {
        let mut child = GroupChild::spawn(&mut detached("exec sleep 60")).expect("spawn");
        let handle = child.group().clone();
        let status = child.kill_and_wait().await.expect("reap the child");
        assert!(!status.success());
        assert_eq!(handle.leader(), None);
        // Neither another wait nor the drop has anything left to signal.
        assert!(!child.wait().await.expect("cached status").success());
        drop(child);
        handle.kill();
    }

    // Given: a running child with a background helper / When: the child is
    // dropped without being awaited / Then: its whole group stops.
    #[tokio::test]
    async fn dropping_a_group_child_stops_its_group() {
        let mut child =
            GroupChild::spawn(&mut detached("sleep 60 & echo $!; wait")).expect("spawn");
        let mut stdout = tokio::io::BufReader::new(child.take_stdout().expect("stdout"));
        let mut line = String::new();
        stdout
            .read_line(&mut line)
            .await
            .expect("read the helper pid");
        let helper = parse_pid(&line);
        let leader = child.group().leader().expect("the leader's pid");
        drop(child);
        assert_stops(helper);
        assert_stops(leader);
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
}
