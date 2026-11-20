#![cfg(target_os = "linux")]
use process_guard::{GuardChild, GuardRuntime, LaunchSpec, pidfd_open, wire};
use std::{
    io::{BufRead, BufReader, Read, Write},
    os::{
        fd::AsRawFd,
        unix::{net::UnixStream, process::ExitStatusExt},
    },
    path::PathBuf,
    process::{Command, Stdio},
};
fn runtime() -> GuardRuntime {
    GuardRuntime::from_bundle(&PathBuf::from(
        std::env::var_os(process_guard::BUNDLE_ENV).expect("prepare bundle"),
    ))
    .unwrap()
}
fn run(rt: &GuardRuntime, script: &str) -> (GuardChild, UnixStream, BufReader<UnixStream>) {
    let (input, child_input) = UnixStream::pair().unwrap();
    let (output, child_output) = UnixStream::pair().unwrap();
    let mut spec = LaunchSpec::new("/bin/sh").unwrap();
    spec.argv
        .extend([b"-c".to_vec(), script.as_bytes().to_vec()]);
    (
        rt.spawn(
            &spec,
            [
                Stdio::from(child_input.into_owned_fd()),
                Stdio::from(child_output.into_owned_fd()),
                Stdio::null(),
            ],
        )
        .unwrap(),
        input,
        BufReader::new(output),
    )
}
trait IntoOwned {
    fn into_owned_fd(self) -> std::os::fd::OwnedFd;
}
impl IntoOwned for UnixStream {
    fn into_owned_fd(self) -> std::os::fd::OwnedFd {
        self.into()
    }
}
fn line(r: &mut impl BufRead) -> String {
    let mut s = String::new();
    assert!(r.read_line(&mut s).unwrap() > 0);
    s.trim().to_owned()
}
fn wait_pid(pid: i32) -> i32 {
    let mut status = 0;
    loop {
        let n = unsafe { libc::waitpid(pid, &mut status, 0) };
        if n == pid {
            return status;
        }
        assert_eq!(
            std::io::Error::last_os_error().kind(),
            std::io::ErrorKind::Interrupted
        );
    }
}
#[test]
fn status_errno_stdio_and_wait_reservation() {
    let rt = runtime();
    for script in [
        "exit 0",
        "exit 37",
        "kill -TERM $$",
        "kill -KILL $$",
        "ulimit -c 0; kill -ABRT $$",
    ] {
        let expected = Command::new("/bin/sh")
            .args(["-c", script])
            .status()
            .unwrap();
        let (mut child, _, _) = run(&rt, script);
        assert_eq!(
            child.wait().unwrap().into_raw(),
            expected.into_raw(),
            "{script}"
        );
        assert_eq!(child.wait().unwrap().into_raw(), expected.into_raw());
    }
    let temp = tempfile::tempdir().unwrap();
    let noexec = temp.path().join("noexec");
    std::fs::write(&noexec, "x").unwrap();
    let badelf = temp.path().join("badelf");
    std::fs::write(&badelf, "not executable format").unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&badelf, std::fs::Permissions::from_mode(0o555)).unwrap();
    for path in [temp.path().join("missing"), noexec, badelf] {
        let expected = Command::new(&path).spawn().unwrap_err().raw_os_error();
        let spec = LaunchSpec::new(path).unwrap();
        let actual = rt
            .spawn(&spec, [Stdio::null(), Stdio::null(), Stdio::null()])
            .unwrap_err()
            .raw_os_error();
        assert_eq!(actual, expected);
    }
    let mut spec = LaunchSpec::new("/bin/sh").unwrap();
    spec.cwd = b"/nonexistent/cwd/guard-test".to_vec();
    assert_eq!(
        rt.spawn(&spec, [Stdio::null(), Stdio::null(), Stdio::null()])
            .unwrap_err()
            .raw_os_error(),
        Some(libc::ENOENT)
    );
    let (mut child, mut input, mut output) = run(&rt, "echo READY; read gate; exit 37");
    assert_eq!(line(&mut output), "READY");
    assert!(child.try_wait().unwrap().is_none());
    child.request_wait().unwrap();
    input.write_all(b"GO\n").unwrap();
    assert_eq!(child.wait().unwrap().code(), Some(37));
}
#[test]
fn wnowait_anchor_then_group_kill_not_root_exit() {
    // Only the outer test controller adopts fixture descendants. The product
    // guard never becomes subreaper, and waits for exactly its target PID.
    assert_eq!(unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1) }, 0);
    let rt = runtime();
    let (mut child, mut input, mut output) = run(
        &rt,
        "(echo DESC:$BASHPID; read gate; echo TAIL) <&0 & echo ROOT:$!:$$; exit 37",
    );
    // /bin/sh may not expose BASHPID; root's $! is the authoritative child PID.
    let mut desc = 0;
    let mut root = 0;
    for _ in 0..2 {
        let text = line(&mut output);
        if let Some(p) = text.strip_prefix("ROOT:") {
            let mut ids = p.split(':');
            desc = ids.next().unwrap().parse::<i32>().unwrap();
            root = ids.next().unwrap().parse::<i32>().unwrap();
        }
    }
    assert_eq!(child.target_id(), root as u32);
    let descfd = pidfd_open(desc).unwrap();
    assert_eq!(child.exited().unwrap().code(), Some(37));
    let stat = std::fs::read_to_string(format!("/proc/{root}/stat")).unwrap();
    assert_eq!(
        stat.rsplit_once(") ").unwrap().1.split_whitespace().next(),
        Some("Z"),
        "leader must still be reserved before wait"
    );
    assert_ne!(unsafe { libc::getpgid(child.id() as i32) }, unsafe {
        libc::getpgid(root)
    });
    // RootExited must NOT kill the descendant: it can still produce output.
    input.write_all(b"GO\n").unwrap();
    assert_eq!(line(&mut output), "TAIL");
    wire::poll(&mut [wire::fd(descfd.as_raw_fd(), libc::POLLIN)], None).unwrap();
    assert!(libc::WIFEXITED(wait_pid(desc)));
    assert_eq!(child.wait().unwrap().code(), Some(37));
    assert!(!std::path::Path::new(&format!("/proc/{root}")).exists());
    let (mut child, _input, mut output) = run(
        &rt,
        "(trap '' TERM INT HUP; echo DESC; read gate) <&0 & echo ROOT:$!; exit 37",
    );
    let mut descendant = 0;
    for _ in 0..2 {
        let s = line(&mut output);
        if let Some(p) = s.strip_prefix("ROOT:") {
            descendant = p.parse().unwrap();
        }
    }
    let fd = pidfd_open(descendant).unwrap();
    assert_eq!(child.exited().unwrap().code(), Some(37));
    assert_eq!(child.wait().unwrap().code(), Some(37));
    wire::poll(&mut [wire::fd(fd.as_raw_fd(), libc::POLLIN)], None).unwrap();
    assert_eq!(libc::WTERMSIG(wait_pid(descendant)), libc::SIGKILL);
    assert_eq!(unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 0) }, 0);
}
#[test]
fn half_close_drop_and_guard_lost_are_not_success() {
    let rt = runtime();
    let (mut child, _input, mut output) = run(&rt, "echo READY; read gate");
    assert_eq!(line(&mut output), "READY");
    child.force_kill().unwrap();
    assert_eq!(child.wait().unwrap().signal(), Some(libc::SIGKILL));
    let (child, _input, mut output) = run(&rt, "echo READY; read gate");
    let target = child.target_id();
    assert_eq!(line(&mut output), "READY");
    drop(child);
    assert!(!std::path::Path::new(&format!("/proc/{target}")).exists());
    assert_eq!(unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1) }, 0);
    let (mut child, _input, mut output) = run(&rt, "echo READY; read gate");
    let target = child.target_id() as i32;
    let targetfd = pidfd_open(target).unwrap();
    assert_eq!(line(&mut output), "READY");
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGKILL) }, 0);
    assert!(child.wait().unwrap_err().to_string().contains("GuardLost"));
    drop(child);
    wire::poll(&mut [wire::fd(targetfd.as_raw_fd(), libc::POLLIN)], None).unwrap();
    assert_eq!(libc::WTERMSIG(wait_pid(target)), libc::SIGKILL);
    assert_eq!(unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 0) }, 0);
}
#[test]
fn target_env_is_not_helper_loader_env() {
    let rt = runtime();
    let (output, child_output) = UnixStream::pair().unwrap();
    let mut spec = LaunchSpec::new("/bin/sh").unwrap();
    spec.argv
        .extend([b"-c".to_vec(), b"printf '%s' \"$GUARD_SENTINEL\"".to_vec()]);
    spec.env = vec![
        (b"GUARD_SENTINEL".to_vec(), vec![b'x', 0xff]),
        (b"LD_PRELOAD".to_vec(), b"/nonexistent/sentinel.so".to_vec()),
        (b"LD_LIBRARY_PATH".to_vec(), b"/nonexistent/libs".to_vec()),
    ];
    let mut child = rt
        .spawn(
            &spec,
            [
                Stdio::null(),
                Stdio::from(child_output.into_owned_fd()),
                Stdio::null(),
            ],
        )
        .unwrap();
    let pid = child.id();
    let maps = std::fs::read_to_string(format!("/proc/{pid}/maps")).unwrap();
    assert!(!maps.contains("sentinel.so"));
    assert!(child.wait().unwrap().success());
    let mut bytes = Vec::new();
    output.take(1024).read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, vec![b'x', 0xff]);
}
