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

pub async fn run(command: &mut Command, timeout: Duration) -> Result<String, String> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| e.to_string())?;
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
}
