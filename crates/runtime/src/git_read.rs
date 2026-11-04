//! Read-only git with the limits every caller needs: a bounded read of a git command's
//! output under a deadline, with the environment and configuration that could make a read
//! write, prompt, fetch or run user programs turned off.
//!
//! The reader is blocking and owns its inputs, so a service on an async runtime runs it on
//! the blocking pool ([`GitReader::run_blocking`]). It is shared by the thread start capture
//! ([`crate::thread_start`]) and the pull request checkout resolver
//! ([`crate::pull_requests::checkout`]).

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

/// The most stderr bytes read from a git command.
pub(crate) const STDERR_LIMIT: usize = 64 * 1024;

/// Why a git read gave no output at all (a command that ran and failed has an exit
/// status in [`GitOutput`] instead).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GitReadError {
    /// The deadline passed; the command was killed.
    TimedOut,
    /// Stdout was longer than the limit; the command was killed.
    TooLarge,
    /// The command could not be started, or its output could not be read.
    Failed,
}

pub(crate) struct GitOutput {
    pub(crate) status: ExitStatus,
    pub(crate) stdout: Vec<u8>,
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) stderr: Vec<u8>,
}

/// Runs read-only git commands until `deadline`.
#[derive(Debug, Clone)]
pub(crate) struct GitReader {
    deadline: Instant,
    /// Replaces the user's global config; tests only.
    #[cfg_attr(not(test), allow(dead_code))]
    global_config: Option<PathBuf>,
}

impl GitReader {
    pub(crate) fn new(deadline: Instant) -> Self {
        Self {
            deadline,
            global_config: None,
        }
    }

    /// Reads `path` instead of the user's global git config. Tests use it to isolate a
    /// run from the machine's configuration.
    #[cfg(test)]
    pub(crate) fn with_global_config(mut self, path: PathBuf) -> Self {
        self.global_config = Some(path);
        self
    }

    pub(crate) fn deadline(&self) -> Instant {
        self.deadline
    }

    /// [`Self::run`] on the blocking pool, for a caller on an async runtime.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) async fn run_blocking(
        &self,
        dir: PathBuf,
        arguments: Vec<String>,
        limit: usize,
    ) -> Result<GitOutput, GitReadError> {
        let reader = self.clone();
        tokio::task::spawn_blocking(move || {
            let arguments: Vec<&str> = arguments.iter().map(String::as_str).collect();
            reader.run(&dir, &arguments, limit)
        })
        .await
        .unwrap_or(Err(GitReadError::Failed))
    }

    /// Run a read-only git command, killing it at the deadline or once stdout exceeds `limit`.
    pub(crate) fn run(
        &self,
        dir: &Path,
        arguments: &[&str],
        limit: usize,
    ) -> Result<GitOutput, GitReadError> {
        let deadline = self.deadline;
        let mut command = Command::new("git");
        // Inherited GIT_DIR/GIT_INDEX_FILE must not point the read at another repository.
        for (name, _) in std::env::vars_os() {
            if name.to_string_lossy().starts_with("GIT_") {
                command.env_remove(name);
            }
        }
        // The user's global config stays readable so its core.excludesFile still
        // hides ignored files; everything that could write, prompt, fetch or run
        // user programs is turned off.
        command
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_NO_LAZY_FETCH", "1")
            .env("LC_ALL", "C")
            // `git diff` rewrites a stat-dirty index even without optional locks.
            .args([
                "-c",
                "core.fsmonitor=false",
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "diff.autoRefreshIndex=false",
            ])
            .args(arguments)
            .current_dir(dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(path) = &self.global_config {
            command.env("GIT_CONFIG_GLOBAL", path);
        }
        let mut child = command.spawn().map_err(|_| GitReadError::Failed)?;
        let (sender, streams) = std::sync::mpsc::channel();
        for (index, stream, limit) in [
            (
                0,
                Box::new(child.stdout.take().expect("piped stdout")) as Box<dyn Read + Send>,
                limit,
            ),
            (
                1,
                Box::new(child.stderr.take().expect("piped stderr")),
                STDERR_LIMIT,
            ),
        ] {
            let sender = sender.clone();
            std::thread::spawn(move || {
                let mut bytes = Vec::new();
                let read = stream
                    .take(limit as u64 + 1)
                    .read_to_end(&mut bytes)
                    .map(|_| bytes);
                let _ = sender.send((index, read));
            });
        }
        drop(sender);
        let mut output = [None, None];
        let failure = loop {
            if output.iter().all(Option::is_some) {
                break None;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            match streams.recv_timeout(remaining) {
                Ok((index, Ok(bytes))) if bytes.len() > limit && index == 0 => {
                    break Some(GitReadError::TooLarge);
                }
                Ok((index, Ok(bytes))) => output[index] = Some(bytes),
                Ok((_, Err(_))) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    break Some(GitReadError::Failed);
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    break Some(GitReadError::TimedOut);
                }
            }
        };
        if let Some(reason) = failure {
            let _ = child.kill();
            let _ = child.wait();
            return Err(reason);
        }
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(5))
                }
                Ok(None) | Err(_) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(GitReadError::TimedOut);
                }
            }
        };
        let [stdout, stderr] = output.map(Option::unwrap_or_default);
        Ok(GitOutput {
            status,
            stdout,
            stderr,
        })
    }
}
