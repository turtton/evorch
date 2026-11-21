//! Host-side git for preparing and reading replay workspaces.
//!
//! Like the start-state capture, inherited `GIT_*` variables are dropped and
//! nothing prompts, runs hooks or fsmonitor. The user's system and global
//! config are not read either, so their diff, apply and ignore settings cannot
//! change what is rebuilt or measured.

use sandbox::owned::{OwnedCommand, OwnedSession};
use std::ffi::OsStr;
use std::path::Path;
use std::process::Stdio;

#[derive(Debug, thiserror::Error)]
#[error("git {command} failed: {detail}")]
pub struct GitError {
    command: String,
    detail: String,
}

fn clean_command(dir: &Path) -> OwnedCommand {
    let mut command = OwnedCommand::new("git");
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("GIT_") {
            command.env_remove(name);
        }
    }
    command
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("LC_ALL", "C")
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

/// Runs to completion under a guard-owned group, collecting both streams.
fn output(command: &mut OwnedCommand) -> std::io::Result<std::process::Output> {
    OwnedSession::spawn(command)?.output(None)
}

fn hardened(dir: &Path) -> OwnedCommand {
    let mut command = clean_command(dir);
    command
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .args([
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.excludesFile=/dev/null",
        ]);
    command
}

fn finish(
    arguments: &[&str],
    output: std::io::Result<std::process::Output>,
) -> Result<Vec<u8>, GitError> {
    let fail = |detail: String| GitError {
        command: arguments.join(" "),
        detail,
    };
    let output = output.map_err(|error| fail(error.to_string()))?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(fail(format!(
            "{}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )))
    }
}

pub(crate) fn run(dir: &Path, arguments: &[&str]) -> Result<Vec<u8>, GitError> {
    run_with_env(dir, arguments, &[])
}

pub(crate) fn run_with_env(
    dir: &Path,
    arguments: &[&str],
    env: &[(&str, &OsStr)],
) -> Result<Vec<u8>, GitError> {
    let mut command = hardened(dir);
    command.args(arguments);
    for (name, value) in env {
        command.env(name, value);
    }
    finish(arguments, output(&mut command))
}

pub(crate) fn run_with_input(dir: &Path, arguments: &[&str], input: &[u8]) -> Result<(), GitError> {
    let mut command = hardened(dir);
    command.args(arguments).stdin(Stdio::piped());
    let output = OwnedSession::spawn(&mut command).and_then(|session| session.output(Some(input)));
    finish(arguments, output).map(drop)
}

pub(crate) fn text(dir: &Path, arguments: &[&str]) -> Result<String, GitError> {
    let output = run(dir, arguments)?;
    String::from_utf8(output)
        .map(|text| text.trim().to_owned())
        .map_err(|error| GitError {
            command: arguments.join(" "),
            detail: error.to_string(),
        })
}

/// A value from the user's own git config, which [`run`] deliberately ignores.
pub(crate) fn user_config(dir: &Path, key: &str) -> Option<String> {
    let output = output(clean_command(dir).args(["config", "--get", key])).ok()?;
    let value = String::from_utf8(output.stdout).ok()?.trim().to_owned();
    (output.status.success() && !value.is_empty()).then_some(value)
}
