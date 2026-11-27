//! The inputs an evaluation is approved on and then run on, pinned by hash
//! (ADR 0036 §5, §9). A later change to any of them is detected, not followed:
//! the evaluation is refused, so what the user approved is what runs.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use config::Config;
use runtime::tool_descriptions::BuiltinCatalog;
use serde::{Deserialize, Serialize};
use storage::eval_sample::sha256_hex;

use crate::roles::{self, ModelRef};
use crate::runner;
use crate::source::OriginalThread;
use crate::workspace::{self, CommitIdentity};

/// An overlay as given: its text, and the parsed table.
#[derive(Debug, Clone)]
pub struct Overlay {
    pub text: String,
    pub value: toml::Value,
}

/// The overlays each side loads: the baseline gets the common overlay, the
/// candidate gets the common overlay with the candidate's own over it. There
/// is no candidate side without a candidate overlay.
pub fn compose(
    common: Option<&Overlay>,
    candidate: Option<&Overlay>,
) -> (Option<toml::Value>, Option<toml::Value>) {
    let baseline = common.map(|overlay| overlay.value.clone());
    let candidate = candidate.map(|overlay| match common {
        Some(common) => merge(common.value.clone(), overlay.value.clone()),
        None => overlay.value.clone(),
    });
    (baseline, candidate)
}

/// Tables merge key by key, anything else is replaced: the semantics of the
/// configuration layers.
fn merge(base: toml::Value, over: toml::Value) -> toml::Value {
    match (base, over) {
        (toml::Value::Table(mut base), toml::Value::Table(over)) => {
            for (key, value) in over {
                let merged = match base.remove(&key) {
                    Some(existing) => merge(existing, value),
                    None => value,
                };
                base.insert(key, merged);
            }
            toml::Value::Table(base)
        }
        (_, over) => over,
    }
}

/// Hashes of everything an evaluation's result depends on that can change
/// between approval and the end of the run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Inputs {
    /// The sample's [`storage::eval_sample::EvalSample::content_hash`]; its
    /// checks and their version are part of it.
    pub sample_hash: Option<String>,
    pub common_hash: Option<String>,
    pub candidate_hash: Option<String>,
    /// The effective configuration, project layer included, each side runs on.
    pub baseline_config_hash: String,
    pub candidate_config_hash: Option<String>,
    /// The prompt and preset texts each side's configuration resolves to.
    pub baseline_prompts_hash: String,
    pub candidate_prompts_hash: Option<String>,
}

impl Inputs {
    /// One hash of all of them: what an approval is bound to.
    pub fn digest(&self) -> String {
        sha256_hex(serde_json::to_string(self).unwrap_or_default().as_bytes())
    }

    /// The names of the inputs that differ from `other`.
    pub fn differences(&self, other: &Self) -> Vec<&'static str> {
        let mut names = Vec::new();
        let mut note = |name: &'static str, same: bool| {
            if !same {
                names.push(name);
            }
        };
        note("sample", self.sample_hash == other.sample_hash);
        note("common overlay", self.common_hash == other.common_hash);
        note(
            "candidate overlay",
            self.candidate_hash == other.candidate_hash,
        );
        note(
            "baseline configuration",
            self.baseline_config_hash == other.baseline_config_hash,
        );
        note(
            "candidate configuration",
            self.candidate_config_hash == other.candidate_config_hash,
        );
        note(
            "baseline prompt presets",
            self.baseline_prompts_hash == other.baseline_prompts_hash,
        );
        note(
            "candidate prompt presets",
            self.candidate_prompts_hash == other.candidate_prompts_hash,
        );
        names
    }
}

/// Hash the inputs as they are now, for a thread of `project_root`.
///
/// # Errors
/// A configuration or a preset cannot be resolved.
pub fn collect(
    user_config_dir: Option<&Path>,
    project_root: &Path,
    sample_hash: Option<String>,
    common: Option<&Overlay>,
    candidate: Option<&Overlay>,
) -> Result<Inputs, String> {
    let (baseline_overlay, candidate_overlay) = compose(common, candidate);
    let side = |overlay: Option<&toml::Value>| -> Result<(String, String), String> {
        let config = runner::load_config(user_config_dir, Some(project_root), overlay)
            .map_err(|error| error.to_string())?;
        let value = serde_json::to_value(&config).map_err(|error| error.to_string())?;
        let sources = config::resolve_prompt_sources(&config, user_config_dir)
            .map_err(|error| error.to_string())?;
        Ok((
            sha256_hex(value.to_string().as_bytes()),
            sha256_hex(format!("{sources:?}").as_bytes()),
        ))
    };
    let (baseline_config_hash, baseline_prompts_hash) = side(baseline_overlay.as_ref())?;
    let candidate_side = candidate_overlay
        .as_ref()
        .map(|overlay| side(Some(overlay)));
    let (candidate_config_hash, candidate_prompts_hash) = match candidate_side {
        Some(side) => {
            let (config, prompts) = side?;
            (Some(config), Some(prompts))
        }
        None => (None, None),
    };
    Ok(Inputs {
        sample_hash,
        common_hash: common.map(|overlay| sha256_hex(overlay.text.as_bytes())),
        candidate_hash: candidate.map(|overlay| sha256_hex(overlay.text.as_bytes())),
        baseline_config_hash,
        candidate_config_hash,
        baseline_prompts_hash,
        candidate_prompts_hash,
    })
}

/// What this build is: the evaluation code's version, a hash of the tool
/// descriptions it ships (what a description change is measured against), and
/// a hash of the executable that runs the evaluation, which is what actually
/// identifies the build.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Build {
    pub package: String,
    pub version: String,
    pub tool_descriptions_hash: String,
    /// File name of the running executable; `None` when it cannot be resolved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executable: Option<String>,
    /// SHA-256 of the running executable's bytes; `None` when it cannot be read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executable_sha256: Option<String>,
}

fn running_executable() -> &'static (Option<String>, Option<String>) {
    static EXECUTABLE: std::sync::OnceLock<(Option<String>, Option<String>)> =
        std::sync::OnceLock::new();
    EXECUTABLE.get_or_init(|| {
        let Ok(path) = std::env::current_exe() else {
            return (None, None);
        };
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned());
        (name, storage::eval_sample::sha256_file_hex(&path).ok())
    })
}

impl Build {
    pub fn current(catalog: &BuiltinCatalog) -> Self {
        let descriptions = serde_json::to_string(&catalog.descriptions()).unwrap_or_default();
        let (executable, executable_sha256) = running_executable().clone();
        Self {
            package: env!("CARGO_PKG_NAME").into(),
            version: env!("CARGO_PKG_VERSION").into(),
            tool_descriptions_hash: sha256_hex(descriptions.as_bytes()),
            executable,
            executable_sha256,
        }
    }
}

/// The project configuration a replay will load: that of the workspace
/// rebuilt from the thread's recorded start, not of the live checkout, which
/// may have moved on. Approval, change detection, pinning and the replays all
/// resolve the project layer here. The directory is removed when this drops.
pub struct Probe {
    root: PathBuf,
    /// The project root inside the rebuilt workspace.
    pub project_dir: PathBuf,
}

impl Probe {
    /// # Errors
    /// The workspace cannot be rebuilt from the recorded start.
    pub fn prepare(thread: &OriginalThread) -> Result<Self, String> {
        let mut bytes = [0_u8; 8];
        getrandom::fill(&mut bytes).map_err(|error| error.to_string())?;
        let suffix: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
        let root = std::env::temp_dir().join(format!("evorch-eval-probe-{suffix}"));
        let probe = |root: PathBuf| {
            workspace::prepare(
                &thread.source_toplevel,
                &thread.snapshot,
                &root.join("workspace"),
                &CommitIdentity::from_host(&thread.source_toplevel),
            )
            .map(|workspace| Self {
                project_dir: workspace.project_dir,
                root,
            })
        };
        probe(root.clone()).map_err(|error| {
            let _ = std::fs::remove_dir_all(&root);
            format!(
                "thread {}'s start cannot be rebuilt to read its project configuration: {error}",
                thread.thread_id
            )
        })
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// What the manifest records of the pinned inputs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pins {
    pub inputs: Inputs,
    /// [`Inputs::digest`]: the value an approval carries.
    pub digest: String,
    pub build: Build,
    /// The models each side's starting role resolves to in its configuration;
    /// what the replays actually used is recorded per run.
    pub configured_models: BTreeMap<String, Vec<ModelRef>>,
}

impl Pins {
    pub fn new(
        inputs: Inputs,
        build: Build,
        thread_role: &str,
        baseline: &Config,
        candidate: Option<&Config>,
    ) -> Self {
        let address = roles::address(thread_role);
        let models = |config: &Config| {
            address.map_or_else(Vec::new, |address| roles::role_models(config, address))
        };
        let mut configured_models =
            BTreeMap::from([(runner::BASELINE.to_owned(), models(baseline))]);
        if let Some(candidate) = candidate {
            configured_models.insert(runner::CANDIDATE.to_owned(), models(candidate));
        }
        Self {
            digest: inputs.digest(),
            inputs,
            build,
            configured_models,
        }
    }
}

/// Re-checks the pinned inputs before each replay.
pub struct Guard {
    pub user_config_dir: Option<PathBuf>,
    pub project_root: PathBuf,
    pub common: Option<Overlay>,
    pub candidate: Option<Overlay>,
    pub pinned: Inputs,
    /// Keeps the rebuilt project on disk while the replays re-check it.
    pub probe: Option<std::sync::Arc<Probe>>,
}

impl Guard {
    /// # Errors
    /// An input differs from the pinned one, or cannot be read.
    pub fn verify(&self) -> Result<(), String> {
        let now = collect(
            self.user_config_dir.as_deref(),
            &self.project_root,
            self.pinned.sample_hash.clone(),
            self.common.as_ref(),
            self.candidate.as_ref(),
        )?;
        match self.pinned.differences(&now).as_slice() {
            [] => Ok(()),
            changed => Err(format!(
                "{} changed after the evaluation started; nothing further was run, because the \
                 replays would no longer compare what was approved",
                changed.join(", ")
            )),
        }
    }
}

#[cfg(test)]
mod tests;
