//! Input fixtures: the repository, the files a probe reads, the core's
//! configuration. Everything is generated from the frozen spec with fixed
//! seeds, so both arms get byte-identical inputs (the repository even gets the
//! same commit id).
use serde_json::json;
use std::{
    fs, io,
    path::{Path, PathBuf},
    process::Command,
};

pub use crate::gate::StateSpec;

fn next(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut x = *state;
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

/// `lines` lines of 63 hex digits plus a newline (64 bytes each). The hex
/// alphabet has no upper-case letters, so it never contains the needle `D-7`.
fn hex_lines(seed: u64, lines: usize) -> String {
    let mut state = seed;
    let mut out = String::with_capacity(lines * 64);
    for _ in 0..lines {
        let line = format!(
            "{:016x}{:016x}{:016x}{:016x}",
            next(&mut state),
            next(&mut state),
            next(&mut state),
            next(&mut state)
        );
        out.push_str(&line[..63]);
        out.push('\n');
    }
    out
}

fn git(dir: &Path, args: &[&str]) -> io::Result<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "perf-gate")
        .env("GIT_AUTHOR_EMAIL", "perf-gate@example.invalid")
        .env("GIT_COMMITTER_NAME", "perf-gate")
        .env("GIT_COMMITTER_EMAIL", "perf-gate@example.invalid")
        .env("GIT_AUTHOR_DATE", "2026-01-01T00:00:00+0000")
        .env("GIT_COMMITTER_DATE", "2026-01-01T00:00:00+0000")
        .output()?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// Writes the repository (`files/f0000.txt` ..., `fixture.txt`, `tracked.txt`),
/// commits it clean and returns the commit id.
pub fn repository(project: &Path, spec: &StateSpec) -> io::Result<String> {
    fs::create_dir_all(project.join("files"))?;
    let lines = spec.repo_file_bytes as usize / 64;
    for i in 0..spec.repo_files {
        fs::write(
            project.join(format!("files/f{i:04}.txt")),
            hex_lines(0x5eed_0000 + u64::from(i), lines),
        )?;
    }
    let total = spec.fixture_txt_bytes as usize / 64;
    let mut fixture = hex_lines(0xf1_c7_0e, total);
    // 16 needle lines, 64 bytes each like every other line.
    for k in 0..16usize {
        let at = (k * total / 16) * 64;
        let line = format!("{:<63}\n", format!("D-7 marker {k:04}"));
        fixture.replace_range(at..at + 64, &line);
    }
    fs::write(project.join("fixture.txt"), fixture)?;
    fs::write(project.join("tracked.txt"), "tracked\n")?;
    git(project, &["init", "-q", "-b", "main"])?;
    git(project, &["add", "-A"])?;
    git(
        project,
        &[
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-q",
            "-m",
            "perf-gate fixture",
        ],
    )?;
    if !git(project, &["status", "--porcelain"])?.is_empty() {
        return Err(io::Error::other("fixture repository is not clean"));
    }
    git(project, &["rev-parse", "HEAD"])
}

/// config.toml, the model catalog and the sidebar of one arm.
pub fn configure(root: &Path, project: &Path, provider_url: &str) -> io::Result<()> {
    fs::write(
        root.join("config.toml"),
        format!(
            r#"
[core]
idle_grace_secs = 3600
[providers.local]
type = "openai-compatible"
api_protocol = "openai-completions"
base_url = "{provider_url}"
api_key_env = "B3_TEST_KEY"
models = ["mock-model"]
default_model = "mock-model"
[[routing.routes.worker]]
profile = "local"
[[routing.routes.orchestrator]]
profile = "local"
"#
        ),
    )?;
    let cache = root.join("cache/evorch");
    fs::create_dir_all(&cache)?;
    fs::copy(
        test_support::model_catalog_dir().join("models-dev.json"),
        cache.join("models-dev.json"),
    )?;
    let mut sidebar = workspace_state::SidebarState::default();
    sidebar
        .add_project(workspace_state::ProjectId::new("p"), "perf", project)
        .map_err(io::Error::other)?;
    fs::write(root.join("sidebar.json"), serde_json::to_vec(&sidebar)?)
}

/// User text of a history turn: the marker, padded to exactly `bytes` bytes.
pub fn padded(marker: &str, bytes: usize) -> String {
    let mut text = format!("{marker} ");
    while text.len() < bytes {
        text.push('u');
    }
    text.truncate(bytes);
    text
}

pub fn project_dir(root: &Path) -> PathBuf {
    root.join("project")
}

/// Digest of what both arms are given, for conditions.json.
pub fn describe(spec: &StateSpec, commit: &str) -> serde_json::Value {
    json!({
        "repo_commit": commit,
        "repo_files": spec.repo_files,
        "repo_file_bytes": spec.repo_file_bytes,
        "fixture_txt_bytes": spec.fixture_txt_bytes,
    })
}
