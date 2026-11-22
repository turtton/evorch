//! Synthetic typed PTY API coverage, not real-core B3-06/07 evidence.
#![cfg(target_os = "linux")]
use sandbox::{
    ChildKind, SpawnLedger,
    owned::{OwnedPtyChild, OwnedPtyCommand},
};
use std::{
    io::{BufRead, BufReader, Read, Write},
    os::{
        fd::{AsRawFd, OwnedFd},
        unix::process::ExitStatusExt,
    },
    path::{Path, PathBuf},
    process::Command,
};
fn size() -> portable_pty::PtySize {
    portable_pty::PtySize {
        rows: 24,
        cols: 80,
        pixel_width: 0,
        pixel_height: 0,
    }
}
fn fixture(name: &str, output: &Path, shared: bool) {
    let mut c = Command::new(std::env::var_os("CC").unwrap_or_else(|| "cc".into()));
    c.args(["-Wall", "-Wextra", "-Werror"]);
    if shared {
        c.args(["-shared", "-fPIC"]);
    }
    assert!(
        c.arg("-o")
            .arg(output)
            .arg(
                PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap())
                    .join("fixtures")
                    .join(name)
            )
            .status()
            .unwrap()
            .success()
    );
}
fn line(output: &mut impl BufRead, prefix: &str) -> String {
    loop {
        let mut s = String::new();
        assert!(
            output.read_line(&mut s).unwrap() > 0,
            "PTY EOF before {prefix}"
        );
        if s.starts_with(prefix) {
            return s.trim().into();
        }
    }
}
fn signal(fd: &impl AsRawFd) {
    unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            fd.as_raw_fd(),
            libc::SIGKILL,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        );
    }
}
fn dead(fd: &impl AsRawFd) {
    process_guard::wire::poll(
        &mut [process_guard::wire::fd(fd.as_raw_fd(), libc::POLLIN)],
        None,
    )
    .unwrap();
}
struct Reaper(Vec<(i32, OwnedFd)>);
// A /proc children snapshot can race another owner reaping its child. Validate
// the opened identity, not its numeric PID, before signalling a discovered peer.
fn signal_adopted_child(fd: &OwnedFd) -> bool {
    let mut info = unsafe { std::mem::zeroed::<libc::siginfo_t>() };
    loop {
        if unsafe {
            libc::waitid(
                libc::P_PIDFD,
                fd.as_raw_fd() as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        } == 0
        {
            signal(fd);
            return true;
        }
        if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            return false;
        }
    }
}
impl Reaper {
    fn new() -> Self {
        assert_eq!(unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1) }, 0);
        // This isolated controller is not its own child. A broken ownership
        // check kills only this controller and makes the outer test fail.
        let own = process_guard::pidfd_open(std::process::id() as i32).unwrap();
        assert!(!signal_adopted_child(&own));
        Self(Vec::new())
    }
    fn add(&mut self, pid: i32) {
        self.0.push((pid, process_guard::pidfd_open(pid).unwrap()));
    }
}
impl Drop for Reaper {
    fn drop(&mut self) {
        for (_, fd) in &self.0 {
            signal(fd);
        }
        // Isolated controller only. Also recover processes created before an assertion.
        loop {
            let mut pids = std::collections::BTreeSet::new();
            for task in std::fs::read_dir("/proc/self/task").unwrap() {
                if let Ok(text) = std::fs::read_to_string(task.unwrap().path().join("children")) {
                    pids.extend(text.split_whitespace().map(|s| s.parse::<i32>().unwrap()));
                }
            }
            if pids.is_empty() {
                break;
            }
            for pid in pids {
                let Ok(fd) = process_guard::pidfd_open(pid) else {
                    continue;
                };
                if !signal_adopted_child(&fd) {
                    continue;
                }
                loop {
                    if unsafe { libc::waitpid(pid, std::ptr::null_mut(), 0) } >= 0
                        || std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted
                    {
                        break;
                    }
                }
            }
        }
    }
}
#[test]
fn typed_pty_contract() {
    let Ok(case) = std::env::var("OWNED_PTY_CASE") else {
        let dir = tempfile::tempdir().unwrap();
        let library = dir.path().join("fail.so");
        fixture("owned-pty-fail.c", &library, true);
        for case in [
            "io",
            "unmanaged",
            "ctrl-c",
            "eof",
            "group",
            "shell-group",
            "drop",
            "escape",
            "lost",
            "errors",
            "reader-error",
            "writer-error",
            "openpty-error",
        ] {
            let mut c = Command::new(std::env::current_exe().unwrap());
            c.args([
                "--exact",
                "typed_pty_contract",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("OWNED_PTY_CASE", case);
            if case == "openpty-error" {
                c.env("LD_PRELOAD", &library);
            }
            let output = c.output().unwrap();
            assert!(
                output.status.success(),
                "{case}: {}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        return;
    };
    let mut reaper = Reaper::new();
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("target");
    fixture("owned-pty-target.c", &target, false);
    let ledger = SpawnLedger::new();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let spawn = |command| {
        runtime.block_on(OwnedPtyChild::spawn_blocking(
            if case == "unmanaged" {
                None
            } else {
                Some(ledger.clone())
            },
            ChildKind::ShellJob,
            Some("pty-test".into()),
            command,
            size(),
        ))
    };
    if case == "errors" {
        let mut c = OwnedPtyCommand::new(dir.path().join("missing"));
        assert_eq!(spawn(c).unwrap_err().raw_os_error(), Some(libc::ENOENT));
        c = OwnedPtyCommand::new(&target);
        c.arg("io");
        ledger.close_fence();
        assert!(spawn(c).is_err());
        assert_eq!(ledger.child_stats().registered, 0);
        return;
    }
    let control_path = dir.path().join("desc.sock");
    let listener = std::os::unix::net::UnixListener::bind(&control_path).unwrap();
    let mut command = if case == "shell-group" {
        let mut c = OwnedPtyCommand::new("/bin/sh");
        c.args([
            "-c",
            "\"$1\" group \"$2\"; code=$?; exit \"$code\"",
            "pty-caller",
        ])
        .arg(&target)
        .arg(&control_path);
        c
    } else {
        let mut c = OwnedPtyCommand::new(&target);
        c.arg(if case == "escape" {
            "escape"
        } else if case == "group" || case == "drop" {
            "group"
        } else {
            "io"
        });
        if case == "group" || case == "drop" {
            c.arg(&control_path);
        }
        c
    };
    command
        .env_clear()
        .env("ONLY", "target")
        .current_dir(dir.path());
    if case == "openpty-error" {
        assert!(spawn(command).is_err());
        assert_eq!(ledger.child_stats().registered, 0);
        return;
    }
    let (mut child, master) = spawn(command).unwrap();
    let root = child.id().unwrap() as i32;
    reaper.add(root);
    let root_death = process_guard::pidfd_open(root).unwrap();
    let mut output = BufReader::new(master.try_clone_reader().unwrap());
    let mut input = master.take_writer().unwrap();
    let descendant = if ["group", "shell-group", "drop", "escape"].contains(&case.as_str()) {
        let s = line(&mut output, "DESC ");
        let p = s.split_whitespace().nth(1).unwrap().parse::<i32>().unwrap();
        reaper.add(p);
        Some((p, process_guard::pidfd_open(p).unwrap()))
    } else {
        None
    };
    let ready = line(&mut output, "READY ");
    eprintln!("{case}: {ready}");
    let ids: Vec<i32> = ready
        .split_whitespace()
        .skip(1)
        .map(|s| s.parse().unwrap())
        .collect();
    if case == "shell-group" {
        reaper.add(ids[0]);
        assert_eq!(ids[1], root);
    } else {
        assert_eq!(ids[0], root);
    }
    let helper = ids[2];
    reaper.add(helper);
    assert_eq!(unsafe { libc::getsid(root) }, helper);
    if case != "shell-group" {
        assert_eq!(ids[1], helper);
    }
    assert_ne!(ids[3], helper);
    assert_eq!(ids[3], ids[4]);
    assert_eq!(ids[5..], [1, 1]);
    if case != "unmanaged" {
        assert_eq!(ledger.children()[0].leader, Some(root));
    }
    assert!(child.try_wait().unwrap().is_none());
    let mut control = if ["group", "shell-group", "drop"].contains(&case.as_str()) {
        Some(listener.accept().unwrap().0)
    } else {
        None
    };
    match case.as_str() {
        "io" | "unmanaged" => {
            master
                .resize(portable_pty::PtySize {
                    rows: 41,
                    cols: 109,
                    ..size()
                })
                .unwrap();
            input.write_all(b"SIZE\n").unwrap();
            assert_eq!(line(&mut output, "SIZE "), "SIZE 41 109");
            input.write_all(b"EXIT\n").unwrap();
            assert_eq!(child.wait().unwrap().into_raw(), 37 << 8);
            assert_eq!(child.try_wait().unwrap().unwrap().into_raw(), 37 << 8);
        }
        "ctrl-c" => {
            input.write_all(&[3]).unwrap();
            assert_eq!(child.wait().unwrap().signal(), Some(libc::SIGINT));
        }
        "eof" => {
            drop(input);
            assert_eq!(line(&mut output, "EOF"), "EOF");
            assert_eq!(child.wait().unwrap().code(), Some(42));
            let mut rest = String::new();
            output.read_to_string(&mut rest).unwrap();
        }
        "group" | "shell-group" | "escape" => {
            input.write_all(b"EXIT\n").unwrap();
            assert_eq!(child.wait().unwrap().code(), Some(37));
            if case == "escape" {
                let (pid, fd) = descendant.as_ref().unwrap();
                assert_ne!(unsafe { libc::getpgid(*pid) }, ids[3]);
                let mut p = [process_guard::wire::fd(fd.as_raw_fd(), libc::POLLIN)];
                assert_eq!(
                    unsafe { libc::poll(p.as_mut_ptr(), 1, 0) },
                    0,
                    "escapee is outside guarantee"
                );
                signal(fd);
            }
        }
        "drop" => drop(child),
        "writer-error" => {
            assert!(master.take_writer().is_err());
            drop(child);
        }
        "reader-error" => {
            let mut old = std::mem::MaybeUninit::<libc::rlimit>::uninit();
            assert_eq!(
                unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, old.as_mut_ptr()) },
                0
            );
            let old = unsafe { old.assume_init() };
            let low = libc::rlimit {
                rlim_cur: 3,
                rlim_max: old.rlim_max,
            };
            assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &low) }, 0);
            let result = master.try_clone_reader();
            assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &old) }, 0);
            assert!(result.is_err());
            drop(child);
        }
        "lost" => {
            let fd = process_guard::pidfd_open(helper).unwrap();
            signal(&fd);
            dead(&fd);
            assert!(child.wait().unwrap_err().to_string().contains("GuardLost"));
            assert_eq!(ledger.child_stats().reaped, 0);
            drop(child);
            assert!(runtime.block_on(ledger.reap_all_children()).is_err());
            assert_eq!(ledger.child_stats().unconfirmed, 1);
            return;
        }
        _ => unreachable!(),
    }
    dead(&root_death);
    if let Some((_, fd)) = descendant {
        if let Some(control) = control.as_mut() {
            // Distinguish actual exit from a surviving descendant without an
            // elapsed-time assertion. A child not killed can ACK this probe;
            // a pending SIGKILL cannot execute it. The outer RAII reaps either.
            let _ = control.write_all(b"P");
            let mut fds = [
                process_guard::wire::fd(fd.as_raw_fd(), libc::POLLIN),
                process_guard::wire::fd(control.as_raw_fd(), libc::POLLIN),
            ];
            process_guard::wire::poll(&mut fds, None).unwrap();
            if fds[1].revents != 0 {
                let mut ack = [0];
                assert_ne!(
                    control.read(&mut ack).unwrap(),
                    1,
                    "same-group descendant ACKed after retirement"
                );
            }
        }
        dead(&fd);
    }
    runtime.block_on(ledger.reap_all_children()).unwrap();
    assert_eq!(ledger.child_stats().reaped, u64::from(case != "unmanaged"));
    assert_eq!(ledger.child_stats().unconfirmed, 0);
}
