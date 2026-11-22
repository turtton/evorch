//! The launch policy of the typed owners (`sandbox::owned`), which every
//! non-PTY child evorch starts gets from its process guard.
//!
//! A child must not reach evorch's controlling terminal (reading it from a
//! background process group stops the child with SIGTTIN for good), must not
//! inherit descriptors evorch holds without `O_CLOEXEC` (the GUI's browser
//! engine leaves many) nor evorch's blocked shutdown signals, and must stop
//! together with its descendants without a reaped group id ever being
//! signalled. The raw `ProcessGroup` handle that pseudo-terminal children keep
//! until B3 is exercised at the end.

use std::io::Read;
use std::process::{Command, Stdio};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use sandbox::owned::{OwnedChild, OwnedCommand, OwnedSession};
use sandbox::{CommandSpec, DirectSandbox, Sandbox};

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

/// An owned `sh -c script` with /dev/null stdin and piped output.
fn owned_sh(script: &str) -> OwnedCommand {
    let mut command = OwnedCommand::new("sh");
    command
        .args(["-c", script])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    command
}

/// stdout of an owned command, run to its end by a guard-owned session.
fn owned_stdout(mut command: OwnedCommand) -> String {
    let output = OwnedSession::spawn(&mut command)
        .expect("start the owned command")
        .output(None)
        .expect("collect the output");
    String::from_utf8_lossy(&output.stdout).into_owned()
}

// Given: evorch runs with a controlling terminal (the test re-runs itself
// inside a pseudo-terminal) / When: children start as owned sessions, owned
// children, wrapped commands and sandboxed sessions / Then: none of them can
// open that terminal, while a raw child can.
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
        owned_stdout(owned_sh(TERMINAL_PROBE)),
        "terminal-detached\n"
    );
    let wrapped = DirectSandbox::new_unchecked()
        .wrap(spec("sh", &["-c", TERMINAL_PROBE]))
        .expect("wrap the probe");
    let mut command = wrapped.to_owned_command();
    command.stdin(Stdio::null()).stdout(Stdio::piped());
    assert_eq!(owned_stdout(command), "terminal-detached\n");

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build the test runtime");
    let output = runtime.block_on(async {
        let mut child = OwnedChild::spawn(&mut owned_sh(TERMINAL_PROBE)).expect("start the child");
        let mut stdout = String::new();
        tokio::io::AsyncReadExt::read_to_string(
            &mut child.take_stdout().expect("child stdout"),
            &mut stdout,
        )
        .await
        .expect("read the probe");
        child.wait().await.expect("reap the child");
        stdout
    });
    assert_eq!(output, "terminal-detached\n");

    let mut session = OwnedSession::spawn_sandboxed(
        None,
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

// Given: a program that does not exist / When: it starts as an owned session
// or child / Then: the launch reports NotFound.
#[tokio::test]
async fn a_missing_program_is_reported_as_not_found() {
    let absent = "/nonexistent/evorch-launch-policy-probe";
    let error =
        OwnedSession::spawn(&mut OwnedCommand::new(absent)).expect_err("the program is missing");
    assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
    let error = OwnedChild::spawn(&mut OwnedCommand::new(
        "evorch-launch-policy-probe-on-no-path",
    ))
    .expect_err("the program is missing");
    assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
    let wrapped = DirectSandbox::new_unchecked()
        .wrap(spec(absent, &[]))
        .expect("wrap the command");
    let error =
        OwnedSession::spawn(&mut wrapped.to_owned_command()).expect_err("the program is missing");
    assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
}

/// Expectations that need `/proc`, `close_range` or a pidfd.
#[cfg(target_os = "linux")]
mod linux {
    use std::io::{BufRead, BufReader};
    use std::os::fd::{AsRawFd, OwnedFd};

    use rustix::process::Pid;
    use sandbox::owned::{OwnedChild, OwnedSession};
    use sandbox::{BwrapConfig, BwrapSandbox, DirectSandbox, Sandbox};
    use tokio::io::{AsyncBufReadExt, AsyncReadExt};

    use super::{owned_sh, owned_stdout, sh, spec, stdout_of};

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
        while running(pid) {
            std::thread::yield_now();
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

    fn pid_of(id: Option<u32>) -> Pid {
        Pid::from_raw(i32::try_from(id.expect("a running target")).unwrap()).expect("a pid")
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
    // as owned sessions, owned children and wrapped commands / Then: none of them
    // sees that descriptor number, while a raw child sees the file.
    #[tokio::test]
    async fn children_do_not_inherit_descriptors_beyond_their_standard_streams() {
        let (_file, fd, name) = inheritable_descriptor();
        let probe = format!("readlink /proc/self/fd/{} || true", fd.as_raw_fd());
        assert!(
            stdout_of(sh(&probe)).contains(&name),
            "without the policy the child inherits the descriptor, or this probe proves nothing"
        );
        let wrapped = DirectSandbox::new_unchecked()
            .wrap(spec("sh", &["-c", &probe]))
            .expect("wrap the probe")
            .to_owned_command();
        for (label, command) in [("OwnedSession", owned_sh(&probe)), ("wrapped", wrapped)] {
            let mut command = command;
            command
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped());
            let seen = owned_stdout(command);
            assert!(
                !seen.contains(&name),
                "{label} leaked the descriptor: {seen}"
            );
        }
        let mut child = OwnedChild::spawn(&mut owned_sh(&probe)).expect("start the child");
        let mut seen = String::new();
        child
            .take_stdout()
            .expect("child stdout")
            .read_to_string(&mut seen)
            .await
            .expect("read the probe");
        child.wait().await.expect("reap the child");
        assert!(
            !seen.contains(&name),
            "OwnedChild leaked the descriptor: {seen}"
        );
    }

    // Given: evorch holds a descriptor without O_CLOEXEC / When: a command runs
    // inside bubblewrap / Then: bubblewrap hands the descriptor on to a command
    // started without the policy, but the owned command sees neither it nor
    // evorch's session.
    #[ignore = "bwrap 実行環境が必要"]
    #[test]
    fn bwrap_commands_do_not_inherit_descriptors() {
        let workspace = tempfile::tempdir_in(
            std::env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"),
        )
        .expect("workspace");
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
        let mut command = wrapped.to_owned_command();
        command
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped());
        let detached = owned_stdout(command);
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

    // Given: an owned child that started a background grandchild / When: the
    // owner is dropped / Then: the child leads its own session and group, the
    // grandchild stays in that group, and both stop.
    #[test]
    fn dropping_an_owned_session_stops_descendants() {
        let mut command = owned_sh("sleep 60 & echo $!; wait");
        let mut session = OwnedSession::spawn(&mut command).expect("start the child");
        let leader = pid_of(session.id());
        let grandchild = read_pid(session.take_stdout().expect("child stdout"));
        assert_eq!(rustix::process::getsid(Some(leader)), Ok(leader));
        assert_eq!(rustix::process::getpgid(Some(leader)), Ok(leader));
        assert_eq!(rustix::process::getpgid(Some(grandchild)), Ok(leader));
        drop(session);
        assert_stops(grandchild);
        assert!(!running(leader));
    }

    // Given: a leader that exits while a background helper keeps its stdout
    // open / When: the child is awaited / Then: the exited leader stays
    // unreaped (a zombie reserving the group id) until the helper is killed,
    // and the output then ends.
    #[tokio::test]
    async fn waiting_kills_the_helpers_of_an_exited_leader_before_reaping_it() {
        let mut child =
            OwnedChild::spawn(&mut owned_sh("sleep 60 & echo $!; exit 0")).expect("spawn");
        let leader = pid_of(child.id());
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
        assert_eq!(child.id(), Some(leader.as_raw_nonzero().get() as u32));

        let status = child.wait().await.expect("reap the leader");
        assert!(status.success());
        assert_eq!(child.id(), None, "the reaped group is retired");
        assert_stops(helper);
        let mut rest = Vec::new();
        stdout
            .read_to_end(&mut rest)
            .await
            .expect("the output ends once the helper is killed");
    }

    // Given: a running child / When: it is killed and reaped, then waited again
    // and dropped / Then: the status is cached and nothing is signalled later.
    #[tokio::test]
    async fn kill_and_wait_retires_the_group_before_a_later_drop() {
        let mut child = OwnedChild::spawn(&mut owned_sh("exec sleep 60")).expect("spawn");
        let handle = child.group().clone();
        let status = child.kill_and_wait().await.expect("reap the child");
        assert!(!status.success());
        assert_eq!(handle.id(), None);
        assert!(!child.wait().await.expect("cached status").success());
        drop(child);
        handle
            .kill()
            .expect("a retired group has nothing to signal");
    }

    // Given: a running child with a background helper, and a clone of its group
    // capability kept elsewhere (as a shell job keeps one for stop) / When: the
    // child is dropped without being awaited / Then: its whole group stops, and
    // the kept clone has nothing left to signal.
    #[tokio::test]
    async fn dropping_an_owned_child_stops_its_group_and_retires_every_clone() {
        let mut child =
            OwnedChild::spawn(&mut owned_sh("sleep 60 & echo $!; wait")).expect("spawn");
        let mut stdout = tokio::io::BufReader::new(child.take_stdout().expect("stdout"));
        let mut line = String::new();
        stdout
            .read_line(&mut line)
            .await
            .expect("read the helper pid");
        let helper = parse_pid(&line);
        let kept = child.group().clone();
        let leader = pid_of(kept.id());
        drop(child);
        assert_stops(helper);
        assert_stops(leader);
        kept.kill().expect("nothing is left to signal");
    }

    // Given: a stdio server that started a background helper / When: the session
    // is dropped / Then: the helper stops with the server.
    #[test]
    fn dropping_a_sandboxed_session_stops_its_descendants() {
        let mut session = OwnedSession::spawn_sandboxed(
            None,
            &DirectSandbox::new_unchecked(),
            spec("sh", &["-c", "sleep 60 & echo $!; exec cat"]),
        )
        .expect("start the stdio session");
        let helper = read_pid(session.take_stdout().expect("session stdout"));
        drop(session);
        assert_stops(helper);
    }
}

#[cfg(target_os = "linux")]
#[test]
fn ordinary_children_do_not_inherit_core_shutdown_signal_mask() {
    struct Restore(libc::sigset_t);
    impl Drop for Restore {
        fn drop(&mut self) {
            assert_eq!(
                unsafe { libc::pthread_sigmask(libc::SIG_SETMASK, &self.0, std::ptr::null_mut()) },
                0
            );
        }
    }
    let mut blocked: libc::sigset_t = unsafe { std::mem::zeroed() };
    let mut old: libc::sigset_t = unsafe { std::mem::zeroed() };
    unsafe {
        libc::sigemptyset(&mut blocked);
        libc::sigaddset(&mut blocked, libc::SIGINT);
        libc::sigaddset(&mut blocked, libc::SIGTERM);
        assert_eq!(
            libc::pthread_sigmask(libc::SIG_BLOCK, &blocked, &mut old),
            0
        );
    }
    let _restore = Restore(old);
    fn child_mask(owned: bool) -> u64 {
        let stdout = if owned {
            let mut child = OwnedCommand::new("cat");
            child
                .arg("/proc/self/status")
                .stdin(Stdio::null())
                .stdout(Stdio::piped());
            owned_stdout(child)
        } else {
            let mut child = Command::new("cat");
            child.arg("/proc/self/status");
            let output = child.output().unwrap();
            assert!(output.status.success());
            String::from_utf8(output.stdout).unwrap()
        };
        stdout
            .lines()
            .find_map(|s| s.strip_prefix("SigBlk:"))
            .map(|s| u64::from_str_radix(s.trim(), 16).unwrap())
            .unwrap()
    }
    let shutdown = (1 << (libc::SIGINT - 1)) | (1 << (libc::SIGTERM - 1));
    assert_eq!(
        child_mask(false) & shutdown,
        shutdown,
        "positive control inherits the blocked mask"
    );
    assert_eq!(
        child_mask(true) & shutdown,
        0,
        "ordinary child can receive INT/TERM"
    );
}
