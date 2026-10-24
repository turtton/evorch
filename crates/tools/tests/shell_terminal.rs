//! Shell commands must not share evorch's controlling terminal.
//!
//! A command in a background process group of that terminal is stopped by
//! SIGTTIN as soon as it reads `/dev/tty` (for example a git credential
//! prompt), and then never finishes. Detached commands get an error instead.

use std::io::Read;
use std::sync::Arc;

use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use sandbox::DirectSandbox;
use serde_json::json;
use tools::{Shell, Tool, ToolExecutionContext};

const CHILD_FLAG: &str = "TOOLS_SHELL_TERMINAL_CHILD";
const TEST_NAME: &str = "shell_commands_cannot_reach_the_evorch_terminal";
/// Opening `/dev/tty` only succeeds while the command has a controlling
/// terminal; unlike reading it, opening never stops the command.
const PROBE: &str = "if (exec 3</dev/tty) 2>/dev/null; then echo terminal-reachable; else echo terminal-detached; fi";

// Given: evorch runs with a controlling terminal (the test re-runs itself
// inside a pseudo-terminal) / When: the shell tool runs a command
// synchronously and as a job / Then: neither command can open that terminal.
#[test]
fn shell_commands_cannot_reach_the_evorch_terminal() {
    if std::env::var_os(CHILD_FLAG).is_some() {
        run_probes_with_terminal();
        return;
    }
    let pair = native_pty_system()
        .openpty(PtySize::default())
        .expect("open a pseudo-terminal");
    let mut child_test =
        CommandBuilder::new(std::env::current_exe().expect("locate the test executable"));
    child_test.args(["--exact", TEST_NAME, "--nocapture"]);
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

fn run_probes_with_terminal() {
    assert!(
        std::fs::File::open("/dev/tty").is_ok(),
        "the child test itself must own a controlling terminal"
    );
    // The job probe allocates a durable shell handle; keep that state out of
    // the user's configuration directory.
    let state = tempfile::tempdir().expect("shell handle state directory");
    tools::set_shell_job_handle_dir(state.path().join("shell-job-handles"))
        .expect("isolate shell job handle state");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build the test runtime");
    let shell = Shell::new(Arc::new(DirectSandbox::new_unchecked()));
    let synchronous = runtime
        .block_on(shell.execute(json!({ "command": PROBE })))
        .expect("run the synchronous probe");
    assert_eq!(synchronous.content, "exit_code: 0\nterminal-detached\n");
    // A positive start yield returns as soon as the job completes.
    let context = ToolExecutionContext {
        run_id: "run-terminal-probe".into(),
        thread_id: None,
        call_id: None,
    };
    let job = runtime
        .block_on(
            shell
                .execute_with_context(&context, json!({ "command": PROBE, "yield_ms": 1_800_000 })),
        )
        .expect("run the job probe");
    assert!(
        job.content.contains("status: completed") && job.content.contains("terminal-detached\n"),
        "{}",
        job.content
    );
}
