//! The files an evaluation leaves behind (ADR 0036 §6): one `manifest.json`
//! for the whole evaluation and, per replay, `runs/<label>-<n>/` holding its
//! `evorch.db`, `diff.patch`, `conversation.json` and `outcome.json`.

use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use chrono::{DateTime, Utc};
use config::{EvalBudgetConfig, RoleBindingConfig};
use serde::Serialize;

use crate::conversation::Entry;
use crate::driver::{DeniedRequest, Status};
use crate::source::OriginalThread;
use crate::workspace::RefChange;

pub const MANIFEST_FILE: &str = "manifest.json";
pub const DATABASE_FILE: &str = "evorch.db";
pub const DIFF_FILE: &str = "diff.patch";
pub const CONVERSATION_FILE: &str = "conversation.json";
pub const OUTCOME_FILE: &str = "outcome.json";

/// `<user config dir>/eval/<UTC YYYYmmdd-HHMMSS>-<thread id>`.
pub fn default_dir(user_config_dir: &Path, thread_id: &str, now: SystemTime) -> PathBuf {
    let stamp = DateTime::<Utc>::from(now).format("%Y%m%d-%H%M%S");
    let thread: String = thread_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();
    user_config_dir
        .join("eval")
        .join(format!("{stamp}-{thread}"))
}

/// Create `dir`, refusing one that already holds anything so earlier results
/// are never mixed with or overwritten by a new evaluation.
pub fn create_dir(dir: &Path) -> io::Result<()> {
    match std::fs::read_dir(dir) {
        Ok(mut entries) => {
            if entries.next().is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("output directory {} is not empty", dir.display()),
                ));
            }
            Ok(())
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => std::fs::create_dir_all(dir),
        Err(error) => Err(error),
    }
}

pub fn timestamp(at: SystemTime) -> String {
    DateTime::<Utc>::from(at).to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

#[derive(Debug, Clone, Serialize)]
pub struct StartSummary {
    pub project_root: PathBuf,
    pub repository: PathBuf,
    pub repo_prefix: String,
    pub head_commit: String,
    pub head_ref: Option<String>,
    pub diff_bytes: usize,
    pub untracked_entries: usize,
    pub captured_at_ns: i64,
}

impl StartSummary {
    pub fn of(thread: &OriginalThread) -> Self {
        Self {
            project_root: thread.project_root.clone(),
            repository: thread.source_toplevel.clone(),
            repo_prefix: thread.snapshot.repo_prefix.clone(),
            head_commit: thread.snapshot.head_commit.clone(),
            head_ref: thread.snapshot.head_ref.clone(),
            diff_bytes: thread.snapshot.diff.len(),
            untracked_entries: thread.snapshot.untracked.len(),
            captured_at_ns: thread.captured_at_ns,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SimulatorSummary {
    /// Where the binding came from: `eval.user_simulator`, or the worker
    /// binding because `eval.user_simulator` is unset.
    pub source: String,
    pub binding: RoleBindingConfig,
}

#[derive(Debug, Clone, Serialize)]
pub struct RunEntry {
    pub label: String,
    pub index: u32,
    pub dir: String,
    /// The random run identifier placed in the system prompt, so cached
    /// answers from another replay cannot be reused.
    pub nonce: String,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub status: Option<Status>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Manifest {
    pub thread_id: String,
    pub source_db: PathBuf,
    pub start: StartSummary,
    /// The candidate overlay as given, or `None` when only the baseline runs.
    pub candidate_overlay: Option<String>,
    pub configurations: Vec<String>,
    pub repeat: u32,
    pub simulator: SimulatorSummary,
    pub budget: EvalBudgetConfig,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub runs: Vec<RunEntry>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Outcome {
    pub status: Status,
    pub reason: String,
    pub user_turns: u32,
    pub tokens: u64,
    pub tool_calls: u32,
    pub elapsed_ms: u64,
    pub denied_requests: Vec<DeniedRequest>,
    pub pushed_refs: Vec<RefChange>,
}

/// Write `value` as pretty JSON through a temporary file, so a reader never
/// sees a partly written file.
pub fn write_json(path: &Path, value: &impl Serialize) -> io::Result<()> {
    let mut text = serde_json::to_vec_pretty(value).map_err(io::Error::other)?;
    text.push(b'\n');
    let partial = path.with_extension("json.partial");
    std::fs::write(&partial, text)?;
    std::fs::rename(partial, path)
}

pub fn write_run(
    dir: &Path,
    outcome: &Outcome,
    conversation: &[Entry],
    diff: Option<&[u8]>,
) -> io::Result<()> {
    if let Some(diff) = diff {
        std::fs::write(dir.join(DIFF_FILE), diff)?;
    }
    write_json(&dir.join(CONVERSATION_FILE), &conversation)?;
    write_json(&dir.join(OUTCOME_FILE), outcome)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn default_dir_is_stamped_in_utc_and_keeps_thread_ids_path_safe() {
        // Given: a fixed instant and a thread id with path separators.
        let at = SystemTime::UNIX_EPOCH + Duration::from_secs(1_792_454_400);

        // When: the default output directory is derived.
        let dir = default_dir(Path::new("/config"), "a/b c", at);

        // Then: it sits under eval/ with a UTC stamp and a sanitized id.
        assert_eq!(dir, Path::new("/config/eval/20261020-000000-a_b_c"));
    }

    #[test]
    fn only_missing_or_empty_output_directories_are_used() {
        // Given: a missing, an empty and a non-empty directory.
        let root = tempfile::tempdir().unwrap();
        let missing = root.path().join("new/eval");
        let empty = root.path().join("empty");
        std::fs::create_dir(&empty).unwrap();
        let used = root.path().join("used");
        std::fs::create_dir(&used).unwrap();
        std::fs::write(used.join(MANIFEST_FILE), "{}").unwrap();

        // When / Then: the first two are accepted and the last is refused.
        create_dir(&missing).unwrap();
        assert!(missing.is_dir());
        create_dir(&empty).unwrap();
        assert_eq!(
            create_dir(&used).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
    }
}
