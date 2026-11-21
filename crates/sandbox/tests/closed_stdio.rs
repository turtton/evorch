//! Owned launches started by a process whose own stdio is closed.
//!
//! The harness starts this test binary as a role with descriptors 0, 1 and 2
//! closed before exec. After Rust startup, the role explicitly closes selected
//! descriptors again and verifies `EBADF` immediately before each owned launch.
//! It starts the representative stream selections the callers use (see
//! "Closed stdio, per path" in `sandbox/src/owned/README.md`):
//! all pipes, `/dev/null` stdin with piped output, files for both output streams
//! (eval groups) and a pipe pair with `/dev/null` stderr (the renderer's and the
//! quota RPC's shape). Each must behave as its selection says, whatever the
//! launcher's own descriptors are. The role reports through its exit status,
//! because it has no stdout.
//!
//! Rust's runtime reopens an absent standard descriptor as `/dev/null` at startup
//! and the guard helper normalizes it as well (`process-guard`
//! `closed_stdio_is_sanitized_to_dev_null_by_helper_startup`); this test proves
//! that no caller's selection depends on the launcher's descriptors.
#![cfg(target_os = "linux")]

use std::io::{Read, Write};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};

use sandbox::owned::{OwnedChild, OwnedCommand, OwnedSession};

const ROLE: &str = "SANDBOX_CLOSED_STDIO_ROLE";
const TEST: &str = "an_owned_launch_keeps_its_stream_selection_when_the_launchers_stdio_is_closed";

fn sh(script: &str) -> OwnedCommand {
    let mut command = OwnedCommand::new("/bin/sh");
    command.args(["-c", script]);
    command
}

fn restore_stdio() {
    let null = b"/dev/null\0";
    let opened = unsafe { libc::open(null.as_ptr().cast(), libc::O_RDWR | libc::O_CLOEXEC) };
    assert!(opened >= 0);
    for fd in 0..=2 {
        if opened != fd {
            assert_eq!(unsafe { libc::dup2(opened, fd) }, fd);
        }
    }
    if opened > 2 {
        assert_eq!(unsafe { libc::close(opened) }, 0);
    }
}

fn close_stdio(mask: u8) {
    for fd in 0..=2 {
        if mask & (1 << fd) != 0 {
            assert_eq!(unsafe { libc::close(fd) }, 0);
        }
    }
}

fn assert_stdio_mask(mask: u8) {
    for fd in 0..=2 {
        let result = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        if mask & (1 << fd) != 0 {
            assert_eq!(result, -1, "fd {fd} should be closed");
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::EBADF),
                "fd {fd} must be EBADF immediately before launch"
            );
        } else {
            assert!(result >= 0, "fd {fd} should remain open");
        }
    }
}

/// Runs the role's checks; the first failing one is the exit status.
fn role() -> i32 {
    // stdin, stdout and stderr all piped (LSP, js-worker, MCP, sandbox sessions).
    let mut command = sh("read line; echo \"in:$line\"; echo err >&2");
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    close_stdio(0b111);
    assert_stdio_mask(0b111);
    let output = OwnedSession::spawn(&mut command)
        .unwrap()
        .output(Some(b"hello\n"))
        .unwrap();
    if output.stdout != b"in:hello\n" || output.stderr != b"err\n" {
        return 10;
    }
    assert_stdio_mask(0b111);

    // /dev/null stdin, piped output (git, grep, PR source, verify, GUI slash): the
    // child sees EOF at once instead of waiting on the launcher's descriptor.
    restore_stdio();
    let mut command = sh("if read line; then echo data; else echo eof; fi");
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    close_stdio(0b010);
    assert_stdio_mask(0b010);
    let output = OwnedSession::spawn(&mut command)
        .unwrap()
        .output(None)
        .unwrap();
    if output.stdout != b"eof\n" {
        return 11;
    }
    assert_stdio_mask(0b010);

    // Files for both output streams (eval group members).
    restore_stdio();
    let dir = tempfile::tempdir().unwrap();
    let (out_path, err_path) = (dir.path().join("out.log"), dir.path().join("err.log"));
    let mut command = sh("echo to-out; echo to-err >&2");
    command
        .stdin(Stdio::null())
        .stdout(std::fs::File::create(&out_path).unwrap())
        .stderr(std::fs::File::create(&err_path).unwrap());
    close_stdio(0b101);
    assert_stdio_mask(0b101);
    let mut session = OwnedSession::spawn(&mut command).unwrap();
    if !session.wait().unwrap().success() {
        return 12;
    }
    assert_stdio_mask(0b101);
    if std::fs::read(&out_path).unwrap() != b"to-out\n"
        || std::fs::read(&err_path).unwrap() != b"to-err\n"
    {
        return 13;
    }

    // A piped stdin/stdout pair with /dev/null stderr (renderer IPC, quota RPC).
    restore_stdio();
    let mut command = sh("read line; echo \"pong:$line\"; echo lost >&2");
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    close_stdio(0b011);
    assert_stdio_mask(0b011);
    let mut session = OwnedSession::spawn(&mut command).unwrap();
    let mut stdin = session.take_stdin().unwrap();
    let mut stdout = session.take_stdout().unwrap();
    stdin.write_all(b"ping\n").unwrap();
    drop(stdin);
    let mut reply = String::new();
    stdout.read_to_string(&mut reply).unwrap();
    if reply != "pong:ping\n" || !session.wait().unwrap().success() {
        return 14;
    }
    drop(stdout);
    drop(session);
    assert_stdio_mask(0b011);

    // The default selection inherits the role's descriptors. Exercise every
    // single/combined closed-fd boundary after the Rust role has started.
    restore_stdio();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .build()
        .unwrap();
    for mask in 1..=0b111 {
        restore_stdio();
        let mut command = sh("exit 37");
        close_stdio(mask);
        assert_stdio_mask(mask);
        let mut child = match runtime.block_on(async { OwnedChild::spawn(&mut command) }) {
            Ok(child) => child,
            Err(_) => return 15,
        };
        if runtime
            .block_on(child.wait())
            .ok()
            .and_then(|status| status.code())
            != Some(37)
        {
            return 16;
        }
        assert_stdio_mask(mask);
    }
    0
}

// Given: a launcher whose descriptors 0, 1 and 2 are closed / When: it starts
// owned children with each caller's stream selection / Then: every child gets
// exactly the streams its selection names.
#[test]
fn an_owned_launch_keeps_its_stream_selection_when_the_launchers_stdio_is_closed() {
    if std::env::var_os(ROLE).is_some() {
        std::process::exit(role());
    }
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", TEST, "--nocapture"])
        .env(ROLE, "1");
    // SAFETY: the hook only closes descriptors, which is async-signal-safe.
    unsafe {
        command.pre_exec(|| {
            for fd in 0..3 {
                libc::close(fd);
            }
            Ok(())
        });
    }
    let status = command.status().expect("start the role");
    assert_eq!(status.code(), Some(0), "the role failed: {status:?}");
}
