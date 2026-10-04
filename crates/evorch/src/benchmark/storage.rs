use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use async_trait::async_trait;
use event_bus::{AgentRunPhase, Event, EventKind, UsageEvent};
use runtime::benchmark::{BenchmarkCheckpoint, BenchmarkRecorder};
use runtime::snapshot::{SnapshotId, SnapshotStore};
use serde::{Deserialize, Serialize};

use super::{BenchmarkResult, TaskSpec, invalid};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Metrics {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub elapsed_ms: u64,
    /// Prices vary by provider; token accounting does not imply measured cost.
    pub cost: Option<f64>,
}

impl Metrics {
    pub fn from_events(events: &[Event], elapsed_ms: u64) -> Self {
        let mut metrics = Self {
            elapsed_ms,
            ..Self::default()
        };
        for event in events {
            if let EventKind::Usage(UsageEvent::Usage {
                input_tokens,
                output_tokens,
                cache_read_tokens,
                cache_write_tokens,
                ..
            }) = &event.kind
            {
                metrics.input_tokens = metrics.input_tokens.saturating_add(*input_tokens);
                metrics.output_tokens = metrics.output_tokens.saturating_add(*output_tokens);
                metrics.cache_read_tokens =
                    metrics.cache_read_tokens.saturating_add(*cache_read_tokens);
                metrics.cache_write_tokens = metrics
                    .cache_write_tokens
                    .saturating_add(*cache_write_tokens);
            }
        }
        metrics
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Evaluation {
    pub passed: bool,
    pub output: String,
    pub verifier_tampered: Vec<PathBuf>,
    pub unexpected_changes: Vec<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Trial {
    pub name: String,
    pub candidate: Option<runtime::ModelPreference>,
    pub phase: AgentRunPhase,
    pub final_text: Option<String>,
    pub error: Option<String>,
    pub diff: String,
    pub metrics: Metrics,
    pub evaluation: Evaluation,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Baseline {
    pub version: u32,
    pub spec: TaskSpec,
    pub workspace: PathBuf,
    pub user_config_dir: Option<PathBuf>,
    pub initial_snapshot: String,
    pub checkpoint_snapshot: Option<String>,
    pub checkpoint: Option<BenchmarkCheckpoint>,
    pub local: Option<Trial>,
    pub full: Trial,
    pub protected: BTreeMap<PathBuf, Vec<u8>>,
    pub original_files: BTreeMap<PathBuf, Vec<u8>>,
    /// Terminal failure may leave process teardown unproven. Never reuse this
    /// workspace even if the companion poison marker is manually deleted.
    pub poisoned: Option<String>,
}

pub(crate) fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> BenchmarkResult<T> {
    Ok(serde_json::from_slice(&std::fs::read(path)?)?)
}

pub(crate) fn write_json(path: &Path, value: &impl Serialize) -> BenchmarkResult<()> {
    // Atomic replacement keeps a crashed trial visibly incomplete.
    let temporary = path.with_extension("json.pending");
    std::fs::write(&temporary, serde_json::to_vec_pretty(value)?)?;
    std::fs::rename(temporary, path)?;
    Ok(())
}

/// Copy source files only. Symlinks and special files cannot acquire authority
/// over the original fixture or escape the benchmark workspace.
pub(crate) fn copy_fixture(source: &Path, destination: &Path) -> BenchmarkResult<()> {
    std::fs::create_dir(destination)?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let name = entry.file_name();
        if name == ".git" || name == "target" {
            continue;
        }
        let kind = entry.file_type()?;
        if kind.is_dir() {
            copy_fixture(&entry.path(), &destination.join(name))?;
        } else if kind.is_file() {
            std::fs::copy(entry.path(), destination.join(name))?;
        } else {
            return Err(invalid(format!(
                "fixture contains a symlink or special file: {}",
                entry.path().display()
            )));
        }
    }
    Ok(())
}

/// Git's snapshot index omits repository metadata and treats embedded repos as
/// gitlinks. Reject newly created repositories instead of carrying future refs
/// into another frozen replay.
pub(crate) fn reject_git_metadata(workspace: &Path) -> BenchmarkResult<()> {
    for entry in std::fs::read_dir(workspace)? {
        let entry = entry?;
        if entry.file_name() == ".git" {
            return Err(invalid(format!(
                "benchmark workspace must stay .git-free: {}; retain this failed workspace for inspection before removing the metadata",
                entry.path().display()
            )));
        }
        if entry.file_type()?.is_dir() {
            reject_git_metadata(&entry.path())?;
        }
    }
    Ok(())
}

pub(crate) fn file_inventory(workspace: &Path) -> BenchmarkResult<BTreeMap<PathBuf, Vec<u8>>> {
    fn walk(
        root: &Path,
        directory: &Path,
        files: &mut BTreeMap<PathBuf, Vec<u8>>,
    ) -> BenchmarkResult<()> {
        for entry in std::fs::read_dir(directory)? {
            let entry = entry?;
            let path = entry.path();
            let relative = path.strip_prefix(root)?.to_path_buf();
            if relative == Path::new("target") || relative == Path::new(".benchmark-tool-output") {
                continue;
            }
            let kind = entry.file_type()?;
            if kind.is_dir() {
                walk(root, &path, files)?;
            } else if kind.is_file() {
                files.insert(relative, std::fs::read(path)?);
            } else {
                return Err(invalid(format!(
                    "verification rejects symlink or special file: {}",
                    path.display()
                )));
            }
        }
        Ok(())
    }
    let mut files = BTreeMap::new();
    walk(workspace, workspace, &mut files)?;
    Ok(files)
}

pub(crate) fn protected_files(
    spec: &TaskSpec,
    workspace: &Path,
) -> BenchmarkResult<BTreeMap<PathBuf, Vec<u8>>> {
    let mut files = BTreeMap::new();
    for path in &spec.verifier.protected_paths {
        let full = workspace.join(path);
        if !full.is_file() || !full.canonicalize()?.starts_with(workspace) {
            return Err(invalid(
                "protected verifier file is missing or outside workspace",
            ));
        }
        files.insert(path.clone(), std::fs::read(full)?);
    }
    Ok(files)
}

pub(crate) fn tampered_files(
    protected: &BTreeMap<PathBuf, Vec<u8>>,
    workspace: &Path,
) -> Vec<PathBuf> {
    protected
        .iter()
        .filter_map(|(path, bytes)| {
            let full = workspace.join(path);
            let safe = full
                .canonicalize()
                .is_ok_and(|resolved| resolved.starts_with(workspace));
            (!safe || std::fs::read(full).ok().as_ref() != Some(bytes)).then(|| path.clone())
        })
        .collect()
}

pub(crate) struct CaptureState {
    pub store: SnapshotStore,
    pub checkpoint: Option<BenchmarkCheckpoint>,
    pub snapshot: Option<SnapshotId>,
    pub local: Option<(AgentRunPhase, Option<String>, Option<SnapshotId>)>,
}

pub(crate) struct Recorder {
    pub state: Mutex<CaptureState>,
    pub directory: PathBuf,
}

#[async_trait]
impl BenchmarkRecorder for Recorder {
    async fn capture(&self, checkpoint: &BenchmarkCheckpoint) -> Result<(), String> {
        reject_git_metadata(&checkpoint.workspace_root).map_err(|e| e.to_string())?;
        let mut state = self.state.lock().map_err(|e| e.to_string())?;
        let snapshot = state.store.capture_all().map_err(|e| e.to_string())?;
        write_json(&self.directory.join("checkpoint.json"), checkpoint)
            .map_err(|e| e.to_string())?;
        std::fs::write(
            self.directory.join("checkpoint-snapshot.txt"),
            snapshot.as_str(),
        )
        .map_err(|e| e.to_string())?;
        state.checkpoint = Some(checkpoint.clone());
        state.snapshot = Some(snapshot);
        Ok(())
    }

    async fn completed(
        &self,
        checkpoint: &BenchmarkCheckpoint,
        phase: AgentRunPhase,
        result: Option<&str>,
    ) -> Result<(), String> {
        let mut state = self.state.lock().map_err(|e| e.to_string())?;
        if phase != AgentRunPhase::Done {
            state.local = Some((phase, result.map(str::to_owned), None));
            write_json(&self.directory.join("poisoned.json"), &serde_json::json!({"reason":"selected delegation did not complete; teardown not proven"})).map_err(|e| e.to_string())?;
            return Ok(());
        }
        reject_git_metadata(&checkpoint.workspace_root).map_err(|e| e.to_string())?;
        let snapshot = state.store.capture_all().map_err(|e| e.to_string())?;
        state.local = Some((phase, result.map(str::to_owned), Some(snapshot)));
        Ok(())
    }
}
pub(crate) fn skipped_evaluation() -> Evaluation {
    Evaluation {
        passed: false,
        output:
            "verification skipped: execution did not complete and process teardown is not proven"
                .into(),
        verifier_tampered: vec![],
        unexpected_changes: vec![],
    }
}
