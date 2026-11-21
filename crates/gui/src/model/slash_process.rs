use sandbox::{
    ChildKind,
    owned::{OwnedChild, OwnedCommand},
};
use std::{process::Stdio, time::Duration};
use tokio::io::{AsyncRead, AsyncReadExt};

pub const OUTPUT_LIMIT: usize = 64 * 1024;

async fn read_output(mut pipe: impl AsyncRead + Unpin) -> Result<Vec<u8>, String> {
    let mut output = Vec::new();
    let mut chunk = [0; 4096];
    loop {
        let count = pipe.read(&mut chunk).await.map_err(|e| e.to_string())?;
        if count == 0 {
            return Ok(output);
        }
        if output.len() + count > OUTPUT_LIMIT {
            return Err("external command output limit exceeded".into());
        }
        output.extend_from_slice(&chunk[..count]);
    }
}

/// Run `command` and return its stdout.
///
/// stdin is /dev/null and stdout and stderr are piped: the command depends on no
/// inherited descriptor. User-defined executables run guard-owned in their own
/// session without the GUI's terminal or descriptors. When the command exits, what
/// it left running is killed so held output pipes cannot turn a result into a
/// timeout; leaving early (timeout, output limit, drop) kills the whole group, not
/// only the direct child.
pub async fn run(mut command: OwnedCommand, timeout: Duration) -> Result<String, String> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = OwnedChild::spawn_blocking(None, ChildKind::Other, None, command)
        .await
        .map_err(|e| e.to_string())?;
    let stdout = child.take_stdout().ok_or("missing stdout")?;
    let stderr = child.take_stderr().ok_or("missing stderr")?;
    let work = async {
        let (status, stdout, stderr) = tokio::try_join!(
            async { child.wait().await.map_err(|e| e.to_string()) },
            read_output(stdout),
            read_output(stderr),
        )?;
        if !status.success() {
            return Err(format!(
                "external command failed ({status}): {}",
                String::from_utf8_lossy(&stderr)
            ));
        }
        String::from_utf8(stdout).map_err(|e| e.to_string())
    };
    tokio::time::timeout(timeout, work)
        .await
        .map_err(|_| "external command timed out".to_owned())?
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn sh(script: &str) -> OwnedCommand {
        let mut command = OwnedCommand::new("sh");
        command.args(["-c", script]);
        command
    }

    #[tokio::test]
    async fn output_limit_rejects_unbounded_stdout_and_stderr() {
        for script in ["yes x", "yes x >&2"] {
            let result = run(sh(script), Duration::from_secs(2)).await;
            assert_eq!(result, Err("external command output limit exceeded".into()));
        }
    }

    #[tokio::test]
    async fn timeout_stops_pending_process() {
        let result = run(
            {
                let mut command = OwnedCommand::new("sleep");
                command.arg("60");
                command
            },
            Duration::from_millis(10),
        )
        .await;
        assert_eq!(result, Err("external command timed out".into()));
    }
}

#[cfg(all(test, target_os = "linux"))]
mod linux_tests {
    use super::*;

    fn sh(script: &str) -> OwnedCommand {
        let mut command = OwnedCommand::new("sh");
        command.args(["-c", script]);
        command
    }

    /// Wait until `pid` no longer runs; a zombie waiting for its reaper counts as stopped.
    async fn assert_stops(pid: &str) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
            stat.rsplit_once(')')
                .is_some_and(|(_, rest)| !rest.trim_start().starts_with('Z'))
        }) {
            assert!(
                std::time::Instant::now() < deadline,
                "process {pid} still runs"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn wait_for_pid_file(path: &std::path::Path) -> String {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok(pid) = std::fs::read_to_string(path)
                && pid.ends_with('\n')
            {
                return pid.trim().to_owned();
            }
            assert!(std::time::Instant::now() < deadline, "no pid file");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn timeout_also_stops_background_descendants() {
        // Given: a command that leaves a background helper and outlives its timeout.
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("helper.pid");
        let script = format!("sleep 60 & echo $! > '{}'; wait", pid_file.display());
        let result = run(sh(&script), Duration::from_secs(1)).await;

        // Then: the helper stops with the command, not only the direct child.
        assert_eq!(result, Err("external command timed out".into()));
        assert_stops(&wait_for_pid_file(&pid_file).await).await;
    }

    #[tokio::test]
    async fn a_helper_holding_the_output_does_not_turn_success_into_a_timeout() {
        // Given: a command that succeeds but leaves a helper holding its stdout.
        let started = std::time::Instant::now();
        let result = run(
            sh("sleep 60 & printf '%s' $!; exit 0"),
            Duration::from_secs(30),
        )
        .await;

        // Then: its exit ends the run with its output, and the helper stops.
        let helper = result.expect("the command succeeded");
        assert!(started.elapsed() < Duration::from_secs(20), "{helper}");
        assert!(helper.parse::<u32>().is_ok(), "{helper}");
        assert_stops(&helper).await;
    }

    #[tokio::test]
    async fn dropping_the_run_stops_the_command_and_its_descendants() {
        // Given: a running command with a background helper.
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("helper.pid");
        let script = format!("sleep 60 & echo $! > '{}'; wait", pid_file.display());
        let task = tokio::spawn(async move { run(sh(&script), Duration::from_secs(60)).await });
        let helper = wait_for_pid_file(&pid_file).await;

        // When: the caller drops the run.
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());

        // Then: the helper stops too.
        assert_stops(&helper).await;
    }

    #[tokio::test]
    async fn runs_in_its_own_session_without_the_guis_descriptors() {
        use std::os::fd::AsRawFd;
        // Given: the GUI holds a descriptor without O_CLOEXEC, as CEF does.
        let held = tempfile::NamedTempFile::new().unwrap();
        let fd = rustix::io::fcntl_dupfd_cloexec(&held, 100).unwrap();
        rustix::io::fcntl_setfd(&fd, rustix::io::FdFlags::empty()).unwrap();
        let probe = format!(
            "echo $$; cut -d' ' -f6 /proc/$$/stat; readlink /proc/$$/fd/{} || echo descriptor-closed",
            fd.as_raw_fd()
        );

        // When: the command reports its pid, its session and that descriptor.
        let stdout = run(sh(&probe), Duration::from_secs(10)).await.unwrap();

        // Then: it leads a session of its own and the descriptor did not reach it.
        let lines: Vec<&str> = stdout.lines().collect();
        let [pid, session, descriptor] = lines[..] else {
            panic!("{stdout}");
        };
        assert!(pid.parse::<u32>().is_ok(), "{stdout}");
        assert_eq!(session, pid, "{stdout}");
        assert_eq!(descriptor, "descriptor-closed", "{stdout}");
    }
}
