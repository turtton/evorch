use std::path::{Component, Path, PathBuf};

use runtime::{ModelPreference, benchmark::BenchmarkSelector};
use serde::{Deserialize, Serialize};

use super::{BenchmarkResult, invalid};

pub const USAGE: &str = "usage: evorch benchmark record --spec <json> --output <new-dir> [--user-config <dir>] | replay --record <dir> --trial <new-name> --profile <profile> --model <model> [--user-config <dir>] | report --record <dir>";

#[derive(Debug)]
pub struct BenchmarkArgs {
    pub command: BenchmarkCommand,
    pub user_config_dir: Option<PathBuf>,
}

#[derive(Debug)]
pub enum BenchmarkCommand {
    Record {
        spec: PathBuf,
        output: PathBuf,
    },
    Replay {
        record: PathBuf,
        trial: String,
        candidate: ModelPreference,
    },
    Report {
        record: PathBuf,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskSpec {
    pub fixture_dir: PathBuf,
    pub root_prompt: String,
    pub target: BenchmarkSelector,
    pub verifier: Verifier,
    pub budget: Budget,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Verifier {
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// All files defining the verifier, including harness and test dependencies.
    /// Candidate edits to any of these invalidate the local trial.
    pub protected_paths: Vec<PathBuf>,
    /// Exact relative implementation artifacts that can be imported into a fresh
    /// trusted verifier fixture. All other source/config changes are rejected.
    #[serde(default)]
    pub allowed_changed_paths: Vec<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Budget {
    pub max_tokens: u64,
    pub max_tool_calls: u32,
    pub timeout_seconds: u64,
}

impl TaskSpec {
    pub fn load(path: &Path) -> BenchmarkResult<Self> {
        let path = path.canonicalize()?;
        let mut spec: Self = serde_json::from_slice(&std::fs::read(&path)?)?;
        if !spec.fixture_dir.is_absolute() {
            spec.fixture_dir = path
                .parent()
                .ok_or_else(|| invalid("spec has no parent"))?
                .join(&spec.fixture_dir);
        }
        spec.fixture_dir = spec.fixture_dir.canonicalize()?;
        spec.validate()?;
        Ok(spec)
    }

    pub fn validate(&self) -> BenchmarkResult<()> {
        if self.root_prompt.trim().is_empty()
            || self
                .target
                .role
                .capabilities()
                .check_tool(self.target.role.name(), "delegate")
                == agents::CapabilityDecision::Allowed
            || self.target.occurrence == 0
            || self.budget.max_tokens == 0
            || self.budget.max_tool_calls == 0
            || self.budget.timeout_seconds == 0
            || self.budget.timeout_seconds > 86400
            || self.verifier.program.trim().is_empty()
            || self.verifier.protected_paths.is_empty()
        {
            return Err(invalid(
                "require a prompt, leaf target, 1-based occurrence, positive budgets (timeout up to 86400 seconds) and protected verifier files",
            ));
        }
        if let Some(category) = &self.target.category {
            let valid =
                config::agent_categories::CategoryId::parse(category).is_some_and(|category| {
                    category.role() == self.target.role.name().to_ascii_lowercase()
                        && category.public_guidance().is_some()
                });
            if !valid {
                return Err(invalid(
                    "target category must be public and owned by the selected role",
                ));
            }
        }
        for path in self
            .verifier
            .protected_paths
            .iter()
            .chain(&self.verifier.allowed_changed_paths)
        {
            if !safe_relative(path) {
                return Err(invalid(
                    "protected verifier paths must be relative files without traversal",
                ));
            }
        }
        for path in &self.verifier.allowed_changed_paths {
            if self.verifier.protected_paths.contains(path)
                || matches!(path.components().next(), Some(Component::Normal(name)) if name == "target" || name == ".benchmark-tool-output" || name == ".git")
            {
                return Err(invalid(
                    "allowed artifacts cannot overlap protected files or generated directories",
                ));
            }
        }
        Ok(())
    }
}

pub(crate) fn safe_relative(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && path
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
}

pub(crate) fn valid_trial(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

pub fn parse_args(argv: impl Iterator<Item = String>) -> BenchmarkResult<BenchmarkArgs> {
    let mut argv = argv;
    if argv.next().as_deref() != Some("benchmark") {
        return Err(invalid(USAGE));
    }
    let command = argv.next().ok_or_else(|| invalid(USAGE))?;
    let mut values = std::collections::BTreeMap::new();
    while let Some(flag) = argv.next() {
        let value = argv.next().ok_or_else(|| invalid(USAGE))?;
        if values.insert(flag, value).is_some() {
            return Err(invalid(USAGE));
        }
    }
    let user_config_dir = values.remove("--user-config").map(PathBuf::from);
    let parsed = match command.as_str() {
        "record" => BenchmarkCommand::Record {
            spec: PathBuf::from(values.remove("--spec").ok_or_else(|| invalid(USAGE))?),
            output: PathBuf::from(values.remove("--output").ok_or_else(|| invalid(USAGE))?),
        },
        "replay" => {
            let trial = values.remove("--trial").ok_or_else(|| invalid(USAGE))?;
            if !valid_trial(&trial) {
                return Err(invalid("invalid trial name"));
            }
            BenchmarkCommand::Replay {
                record: PathBuf::from(values.remove("--record").ok_or_else(|| invalid(USAGE))?),
                trial,
                candidate: ModelPreference {
                    profile: values
                        .remove("--profile")
                        .filter(|v| !v.is_empty())
                        .ok_or_else(|| invalid(USAGE))?,
                    model: Some(
                        values
                            .remove("--model")
                            .filter(|v| !v.is_empty())
                            .ok_or_else(|| invalid(USAGE))?,
                    ),
                    reasoning_effort: None,
                },
            }
        }
        "report" => BenchmarkCommand::Report {
            record: PathBuf::from(values.remove("--record").ok_or_else(|| invalid(USAGE))?),
        },
        _ => return Err(invalid(USAGE)),
    };
    if !values.is_empty() {
        return Err(invalid(USAGE));
    }
    Ok(BenchmarkArgs {
        command: parsed,
        user_config_dir,
    })
}
