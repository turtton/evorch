use std::{process::Stdio, time::Duration};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
};

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

/// Run a fresh `command` (each call adds the launch policy to it) and return
/// its stdout.
pub async fn run(command: &mut Command, timeout: Duration) -> Result<String, String> {
    // User-defined executables run in their own session without the GUI's
    // terminal or descriptors. Leaving early (timeout, output limit, drop) kills
    // their whole process group, not only the direct child.
    sandbox::process::apply_launch_policy(command.as_std_mut());
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| e.to_string())?;
    let _group = sandbox::process::ProcessGroupGuard::new(child.id());
    let stdout = child.stdout.take().ok_or("missing stdout")?;
    let stderr = child.stderr.take().ok_or("missing stderr")?;
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

    #[tokio::test]
    async fn output_limit_rejects_unbounded_stdout_and_stderr() {
        for script in ["yes x", "yes x >&2"] {
            let result = run(
                Command::new("sh").args(["-c", script]),
                Duration::from_secs(2),
            )
            .await;
            assert_eq!(result, Err("external command output limit exceeded".into()));
        }
    }

    #[tokio::test]
    async fn timeout_stops_pending_process() {
        let result = run(Command::new("sleep").arg("60"), Duration::from_millis(10)).await;
        assert_eq!(result, Err("external command timed out".into()));
    }

    #[tokio::test]
    async fn timeout_also_stops_background_descendants() {
        // Given: a command that leaves a background helper and outlives its timeout.
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("helper.pid");
        let script = format!("sleep 60 & echo $! > '{}'; wait", pid_file.display());
        let result = run(
            Command::new("sh").args(["-c", &script]),
            Duration::from_secs(1),
        )
        .await;

        // Then: the helper stops with the command, not only the direct child.
        assert_eq!(result, Err("external command timed out".into()));
        let helper = std::fs::read_to_string(&pid_file).unwrap();
        let helper = helper.trim();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while std::fs::read_to_string(format!("/proc/{helper}/stat")).is_ok_and(|stat| {
            stat.rsplit_once(')')
                .is_some_and(|(_, rest)| !rest.trim_start().starts_with('Z'))
        }) {
            assert!(
                std::time::Instant::now() < deadline,
                "helper {helper} still runs"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
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
        let stdout = run(
            Command::new("sh").args(["-c", &probe]),
            Duration::from_secs(10),
        )
        .await
        .unwrap();

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
