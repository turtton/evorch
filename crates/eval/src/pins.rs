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

/// What this build is: the evaluation code's version and a hash of the tool
/// descriptions it ships, which are what a description change is measured
/// against.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Build {
    pub package: String,
    pub version: String,
    pub tool_descriptions_hash: String,
}

impl Build {
    pub fn current(catalog: &BuiltinCatalog) -> Self {
        let descriptions = serde_json::to_string(&catalog.descriptions()).unwrap_or_default();
        Self {
            package: env!("CARGO_PKG_NAME").into(),
            version: env!("CARGO_PKG_VERSION").into(),
            tool_descriptions_hash: sha256_hex(descriptions.as_bytes()),
        }
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
