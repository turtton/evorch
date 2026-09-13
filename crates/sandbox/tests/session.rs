//! Long-lived process and pipe contracts, without protocol framing.

use std::{
    io::{Read, Write},
    process::Command,
    sync::mpsc,
    thread,
    time::Duration,
};

use sandbox::{BwrapConfig, BwrapSandbox, CommandSpec, DirectSandbox, Sandbox, SandboxError};

fn spec(program: &str, args: &[&str]) -> CommandSpec {
    CommandSpec {
        program: program.to_owned(),
        args: args.iter().map(|arg| (*arg).to_owned()).collect(),
        cwd: None,
        extra_env: Vec::new(),
    }
}

fn round_trip(sandbox: &dyn Sandbox, command: CommandSpec) {
    let mut session = sandbox::StdioSession::spawn(sandbox, command).expect("spawn session");
    let mut stdin = session.take_stdin().expect("stdin");
    let mut stdout = session.take_stdout().expect("stdout");
    let mut stderr = session.take_stderr().expect("stderr");
    let drain = thread::spawn(move || std::io::copy(&mut stderr, &mut std::io::sink()));
    let (send, receive) = mpsc::channel();
    let reader = thread::spawn(move || {
        let mut output = [0; 6];
        let result = stdout.read_exact(&mut output).map(|()| output);
        send.send(result).expect("report read");
    });
    // When: send a bounded request while stdout and stderr are drained concurrently.
    stdin.write_all(b"hello\n").expect("write request");
    let result = receive.recv_timeout(Duration::from_secs(5));
    drop(session);
    reader.join().expect("reader thread");
    drain.join().expect("stderr thread").expect("drain stderr");
    // Then: a response arrives without closing stdin or buffering the whole stream.
    assert_eq!(
        result.expect("response deadline").expect("response"),
        *b"hello\n"
    );
}

#[test]
fn echoes_when_cat_receives_a_request() {
    // Given: an explicitly opted-out sandbox, as in existing process tests.
    round_trip(&DirectSandbox::new_unchecked(), spec("cat", &[]));
}

#[test]
fn echoes_when_stderr_exceeds_pipe_capacity() {
    // Given: a server emitting 1 MiB of diagnostics before responding.
    round_trip(
        &DirectSandbox::new_unchecked(),
        spec("sh", &["-c", "head -c 1048576 /dev/zero >&2; exec cat"]),
    );
}

#[test]
fn reaps_when_dropped_with_stdin_still_owned_by_consumer() {
    // Given: cat cannot exit from stdin EOF because the consumer retains its pipe.
    let mut session =
        sandbox::StdioSession::spawn(&DirectSandbox::new_unchecked(), spec("cat", &[]))
            .expect("spawn session");
    let _stdin = session.take_stdin().expect("stdin");
    let pid = session.id();
    // When: destroy the process owner.
    drop(session);
    // Then: kill -0 would succeed for a zombie; failure proves synchronous reaping.
    assert!(
        !Command::new("kill")
            .args(["-0", &pid.to_string()])
            .output()
            .expect("kill probe")
            .status
            .success()
    );
}

struct DeniedSandbox;

impl Sandbox for DeniedSandbox {
    fn wrap(&self, _spec: CommandSpec) -> Result<sandbox::WrappedCommand, SandboxError> {
        Err(SandboxError::InvalidSpec {
            detail: "denied".to_owned(),
        })
    }
}

#[test]
fn fails_closed_when_wrapping_is_denied() {
    // Given: a sandbox rejecting a valid executable / When: request a session.
    let result = sandbox::StdioSession::spawn(&DeniedSandbox, spec("cat", &[]));
    // Then: no fallback execution occurs and the policy error survives.
    assert!(matches!(
        result,
        Err(sandbox::SessionError::Sandbox(
            SandboxError::InvalidSpec { .. }
        ))
    ));
}

#[test]
fn returns_io_error_when_executable_is_missing() {
    // Given: a nonexistent executable / When: request a session.
    let result = sandbox::StdioSession::spawn(
        &DirectSandbox::new_unchecked(),
        spec("/nonexistent/session-command", &[]),
    );
    // Then: spawning is reported as an I/O error.
    assert!(
        matches!(result, Err(sandbox::SessionError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound)
    );
}

#[ignore = "bwrap 実行環境が必要"]
#[test]
fn echoes_when_cat_runs_inside_bwrap() {
    // Given: the same detected bwrap and workspace fixture as existing isolation tests.
    let workspace = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).expect("workspace");
    let sandbox = BwrapSandbox::detect(BwrapConfig::new(workspace.path().to_path_buf()))
        .expect("bwrap required");
    round_trip(&sandbox, spec("cat", &[]));
}
