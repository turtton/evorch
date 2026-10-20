//! The optional verification command, run in a replay's final workspace under
//! the same network-isolated sandbox as the replay's tools (ADR 0036 §7).

use std::collections::VecDeque;
use std::path::Path;
use std::time::{Duration, Instant};

use sandbox::{CommandSpec, Sandbox};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt};

pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(600);
const TAIL_BYTES: usize = 8 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Verification {
    pub command: String,
    pub passed: bool,
    /// `None` when the command did not exit on its own.
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub elapsed_ms: u64,
    pub stdout_tail: String,
    pub stderr_tail: String,
    /// Why the command could not be started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Verification {
    pub(crate) fn unstarted(command: &str, error: String) -> Self {
        Self {
            command: command.to_owned(),
            passed: false,
            exit_code: None,
            timed_out: false,
            elapsed_ms: 0,
            stdout_tail: String::new(),
            stderr_tail: String::new(),
            error: Some(error),
        }
    }
}

/// Run `command` with `sh -c` in `cwd`. It passes only when it exits with 0
/// within `timeout`; the whole process group is killed at the timeout.
pub async fn run(
    sandbox: &dyn Sandbox,
    command: &str,
    cwd: &Path,
    env: Vec<(String, String)>,
    timeout: Duration,
) -> Verification {
    let wrapped = match sandbox.wrap(CommandSpec {
        program: "sh".into(),
        args: vec!["-c".into(), command.to_owned()],
        cwd: Some(cwd.to_path_buf()),
        extra_env: env,
    }) {
        Ok(wrapped) => wrapped,
        Err(error) => return Verification::unstarted(command, error.to_string()),
    };
    let mut process = tokio::process::Command::new(&wrapped.program);
    wrapped.apply_environment(process.as_std_mut());
    process
        .args(&wrapped.args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .process_group(0)
        .kill_on_drop(true);
    if let Some(cwd) = &wrapped.cwd {
        process.current_dir(cwd);
    }
    let started = Instant::now();
    let mut child = match process.spawn() {
        Ok(child) => child,
        Err(error) => return Verification::unstarted(command, error.to_string()),
    };
    let group = child
        .id()
        .and_then(|id| i32::try_from(id).ok())
        .and_then(rustix::process::Pid::from_raw);
    let (mut stdout, mut stderr) = (child.stdout.take(), child.stderr.take());
    let mut out = Tail::default();
    let mut err = Tail::default();
    let finished = tokio::time::timeout(timeout, async {
        let (status, (), ()) = tokio::join!(
            child.wait(),
            drain(stdout.as_mut(), &mut out),
            drain(stderr.as_mut(), &mut err)
        );
        status
    })
    .await;
    let (exit_code, timed_out, error) = match finished {
        Ok(Ok(status)) => (status.code(), false, None),
        Ok(Err(error)) => (None, false, Some(error.to_string())),
        Err(_) => (None, true, None),
    };
    if let Some(group) = group {
        let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
    }
    let _ = tokio::time::timeout(Duration::from_secs(1), child.wait()).await;
    Verification {
        command: command.to_owned(),
        passed: exit_code == Some(0),
        exit_code,
        timed_out,
        elapsed_ms: started.elapsed().as_millis().try_into().unwrap_or(u64::MAX),
        stdout_tail: out.text(),
        stderr_tail: err.text(),
        error,
    }
}

/// The last `TAIL_BYTES` of a stream, marking that earlier output was dropped.
#[derive(Default)]
struct Tail {
    bytes: VecDeque<u8>,
    dropped: bool,
}

impl Tail {
    fn push(&mut self, chunk: &[u8]) {
        self.bytes.extend(chunk);
        let excess = self.bytes.len().saturating_sub(TAIL_BYTES);
        if excess > 0 {
            self.bytes.drain(..excess);
            self.dropped = true;
        }
    }

    fn text(&self) -> String {
        let bytes: Vec<u8> = self.bytes.iter().copied().collect();
        let text = String::from_utf8_lossy(&bytes);
        if self.dropped {
            format!("[earlier output omitted]\n{text}")
        } else {
            text.into_owned()
        }
    }
}

async fn drain(reader: Option<&mut (impl AsyncRead + Unpin)>, tail: &mut Tail) {
    let Some(reader) = reader else {
        return;
    };
    let mut buffer = [0_u8; 8192];
    while let Ok(read) = reader.read(&mut buffer).await {
        if read == 0 {
            break;
        }
        tail.push(&buffer[..read]);
    }
}

#[cfg(test)]
mod tests {
    use sandbox::DirectSandbox;

    use super::*;

    async fn verify(command: &str, timeout: Duration) -> Verification {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("marker"), "present").unwrap();
        run(
            &DirectSandbox::new_unchecked(),
            command,
            dir.path(),
            vec![("EVAL_FLAG".into(), "set".into())],
            timeout,
        )
        .await
    }

    #[tokio::test]
    async fn passing_and_failing_commands_record_their_exit_and_output() {
        // Given / When: a command that reads the workspace and the environment, and one that fails.
        let passed = verify("cat marker; echo \" $EVAL_FLAG\"", DEFAULT_TIMEOUT).await;
        let failed = verify("echo broken >&2; exit 3", DEFAULT_TIMEOUT).await;

        // Then: the first passes in the workspace with the extra environment; the second fails.
        assert!(passed.passed, "{passed:?}");
        assert_eq!(passed.exit_code, Some(0));
        assert_eq!(passed.stdout_tail, "present set\n");
        assert!(!failed.passed);
        assert_eq!(failed.exit_code, Some(3));
        assert_eq!(failed.stderr_tail, "broken\n");
        assert!(!failed.timed_out);
    }

    #[tokio::test]
    async fn a_command_that_outlives_its_timeout_fails() {
        // Given / When: a command that would run far longer than its timeout.
        let verification = verify("echo started; sleep 30", Duration::from_millis(200)).await;

        // Then: it is stopped well before it would end and counted as failed.
        assert!(verification.timed_out);
        assert!(!verification.passed);
        assert_eq!(verification.exit_code, None);
        assert!(verification.elapsed_ms < 30_000);
    }

    #[test]
    fn long_output_keeps_only_its_tail() {
        // Given: more output than the tail holds.
        let mut tail = Tail::default();
        tail.push(&vec![b'a'; TAIL_BYTES]);
        tail.push(b"end");

        // When / Then: the beginning is dropped and marked.
        let text = tail.text();
        assert!(text.starts_with("[earlier output omitted]\n"));
        assert!(text.ends_with("aend"));
        assert_eq!(text.len(), "[earlier output omitted]\n".len() + TAIL_BYTES);
    }
}
