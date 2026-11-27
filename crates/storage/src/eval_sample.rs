//! Threads pinned as evaluation samples (ADR 0036 §1).
//!
//! A sample names a thread with a replayable start state, the problem observed
//! in it and the roles the evaluation focuses on. It can also carry where the
//! problem was seen ([`SourceRef`]), a free tag list and typed, machine
//! checkable conditions ([`Check`]). While the sample exists the start state is
//! kept past its retention.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The version of the grading semantics the checks are written for. A sample
/// stores the version it was saved under; a grader that does not know it must
/// not grade the checks.
pub const CHECK_VERSION: u32 = 1;
/// The most checks one sample carries.
pub const MAX_CHECKS: usize = 32;
/// The most bytes the kind, source references, tags and checks take once
/// serialized. Over the limit a sample is refused, never cut.
pub const MAX_SPEC_BYTES: usize = 64 * 1024;

/// What the sample is kept for.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SampleKind {
    /// A real failure: the checks must catch it in the recorded thread.
    #[default]
    Failure,
    /// A thread that went well, kept to see that a change does not break it.
    Regression,
}

impl SampleKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Failure => "failure",
            Self::Regression => "regression",
        }
    }
}

/// Where a problem was observed. The reference is evidence only: it never has
/// to equal anything in a replay.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum SourceRef {
    /// A run of the thread.
    Run { run_id: String },
    /// A recorded event, by its id in the event log.
    Event { event_id: i64 },
    /// A self-improvement candidate that names a run of the thread.
    Improvement { candidate_id: String },
}

/// The runs a check looks at.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckScope {
    /// Every run of the replay's delegation tree.
    Tree,
    /// Only runs of these roles, as role addresses such as `worker`.
    Roles(Vec<String>),
}

/// Which tool calls a check counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CallRange {
    /// Calls the model issued itself.
    Direct,
    /// Also calls made inside another tool call, such as inside `run_js`.
    IncludingChildren,
}

/// One machine checkable condition. There is no expression language: every
/// check is one of these three shapes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Check {
    /// `tool` succeeded at least `min` and/or at most `max` times.
    SuccessfulCalls {
        tool: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        min: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max: Option<u32>,
        scope: CheckScope,
        calls: CallRange,
    },
    /// `tool` ended in an error at most `max` times.
    ToolErrors {
        tool: String,
        max: u32,
        scope: CheckScope,
        calls: CallRange,
    },
    /// The diagnostic `code` was raised at most `max` times.
    Diagnostics {
        code: String,
        max: u32,
        scope: CheckScope,
    },
}

impl Check {
    #[must_use]
    pub const fn scope(&self) -> &CheckScope {
        match self {
            Self::SuccessfulCalls { scope, .. }
            | Self::ToolErrors { scope, .. }
            | Self::Diagnostics { scope, .. } => scope,
        }
    }
}

/// A pinned evaluation sample.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvalSample {
    pub thread_id: String,
    /// The observed problem, as the user agreed to it.
    pub problem: String,
    /// Role config addresses such as `worker` or `roles.planner`.
    pub focus_roles: Vec<String>,
    pub kind: SampleKind,
    pub source_refs: Vec<SourceRef>,
    /// A free classification. It never changes what a check means.
    pub tags: Vec<String>,
    pub checks: Vec<Check>,
    /// The grading semantics `checks` were written for ([`CHECK_VERSION`]).
    pub check_version: u32,
    pub created_at_ns: i64,
    pub updated_at_ns: i64,
}

/// What is stored in the `spec` column next to the problem and focus roles.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Spec {
    pub kind: SampleKind,
    pub source_refs: Vec<SourceRef>,
    pub tags: Vec<String>,
    pub checks: Vec<Check>,
    pub check_version: Option<u32>,
}

impl EvalSample {
    /// A sample with a problem and focus roles only: no references, tags or checks.
    #[must_use]
    pub fn new(thread_id: String, problem: String, focus_roles: Vec<String>, at_ns: i64) -> Self {
        Self {
            thread_id,
            problem,
            focus_roles,
            kind: SampleKind::default(),
            source_refs: Vec::new(),
            tags: Vec::new(),
            checks: Vec::new(),
            check_version: CHECK_VERSION,
            created_at_ns: at_ns,
            updated_at_ns: at_ns,
        }
    }

    pub(crate) fn spec(&self) -> Spec {
        Spec {
            kind: self.kind,
            source_refs: self.source_refs.clone(),
            tags: self.tags.clone(),
            checks: self.checks.clone(),
            check_version: Some(self.check_version),
        }
    }

    /// The serialized kind, references, tags and checks that [`MAX_SPEC_BYTES`] bounds.
    ///
    /// # Errors
    /// The spec cannot be serialized.
    pub(crate) fn spec_json(&self) -> Result<String, crate::StorageError> {
        serde_json::to_string(&self.spec())
            .map_err(|error| crate::StorageError::Serialization(error.to_string()))
    }

    /// A digest of what an evaluation depends on: the problem, focus roles,
    /// kind, references, tags, checks and their version. Timestamps are left
    /// out, so saving the same sample again does not change it.
    #[must_use]
    pub fn content_hash(&self) -> String {
        let content = serde_json::json!({
            "problem": self.problem,
            "focus_roles": self.focus_roles,
            "spec": self.spec(),
        });
        let digest = Sha256::digest(content.to_string().as_bytes());
        digest.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    /// Check the sample.
    ///
    /// # Errors
    /// Rejects an empty or secret-bearing problem, role, tag, tool, diagnostic
    /// code or reference, an unknown diagnostic code, a check that cannot
    /// fail or contradicts itself, more than [`MAX_CHECKS`] checks, a spec
    /// over [`MAX_SPEC_BYTES`] and a check version this build does not grade.
    pub fn validate(&self) -> Result<(), crate::StorageError> {
        if self.problem.trim().is_empty() {
            return Err(refused("problem is empty"));
        }
        if self.focus_roles.iter().any(|role| role.trim().is_empty()) {
            return Err(refused("focus role is empty"));
        }
        self.validate_spec()?;
        let guard = crate::entity::SecretGuard::from_env();
        guard.check_text("eval_sample", "problem", &self.problem)?;
        self.focus_roles
            .iter()
            .try_for_each(|role| guard.check_text("eval_sample", "focus_roles", role))?;
        self.strings()
            .try_for_each(|(field, text)| guard.check_text("eval_sample", field, text))
    }

    fn validate_spec(&self) -> Result<(), crate::StorageError> {
        if self.check_version != CHECK_VERSION {
            return Err(refused(&format!(
                "check_version {} is not graded by this build (it grades {CHECK_VERSION})",
                self.check_version
            )));
        }
        if self.checks.len() > MAX_CHECKS {
            return Err(refused(&format!(
                "{} checks are over the limit of {MAX_CHECKS}",
                self.checks.len()
            )));
        }
        if self.tags.iter().any(|tag| tag.trim().is_empty()) {
            return Err(refused("tag is empty"));
        }
        for reference in &self.source_refs {
            let empty = match reference {
                SourceRef::Run { run_id } => run_id.trim().is_empty(),
                SourceRef::Event { event_id } => *event_id <= 0,
                SourceRef::Improvement { candidate_id } => candidate_id.trim().is_empty(),
            };
            if empty {
                return Err(refused("source reference names nothing"));
            }
        }
        for (index, check) in self.checks.iter().enumerate() {
            validate_check(check).map_err(|reason| refused(&format!("check {index}: {reason}")))?;
        }
        let size = self.spec_json()?.len();
        if size > MAX_SPEC_BYTES {
            return Err(refused(&format!(
                "kind, source references, tags and checks take {size} bytes, over the limit of {MAX_SPEC_BYTES}"
            )));
        }
        Ok(())
    }

    /// Every free text of the spec, named by its field.
    fn strings(&self) -> impl Iterator<Item = (&'static str, &str)> {
        let refs = self.source_refs.iter().map(|reference| match reference {
            SourceRef::Run { run_id } => ("source_refs", run_id.as_str()),
            SourceRef::Event { .. } => ("source_refs", ""),
            SourceRef::Improvement { candidate_id } => ("source_refs", candidate_id.as_str()),
        });
        let tags = self.tags.iter().map(|tag| ("tags", tag.as_str()));
        let checks = self.checks.iter().flat_map(|check| {
            let name = match check {
                Check::SuccessfulCalls { tool, .. } | Check::ToolErrors { tool, .. } => {
                    ("checks", tool.as_str())
                }
                Check::Diagnostics { code, .. } => ("checks", code.as_str()),
            };
            let roles = match check.scope() {
                CheckScope::Tree => &[][..],
                CheckScope::Roles(roles) => roles.as_slice(),
            };
            std::iter::once(name).chain(roles.iter().map(|role| ("checks", role.as_str())))
        });
        refs.chain(tags).chain(checks)
    }
}

fn validate_check(check: &Check) -> Result<(), String> {
    match check {
        Check::SuccessfulCalls { tool, min, max, .. } => {
            if tool.trim().is_empty() {
                return Err("tool is empty".into());
            }
            match (min, max) {
                (None, None) => return Err("give min, max or both".into()),
                (Some(0), _) => return Err("min must be at least 1".into()),
                (Some(min), Some(max)) if min > max => {
                    return Err(format!("min {min} is above max {max}"));
                }
                _ => {}
            }
        }
        Check::ToolErrors { tool, .. } => {
            if tool.trim().is_empty() {
                return Err("tool is empty".into());
            }
        }
        Check::Diagnostics { code, .. } => {
            if !event_bus::event::diagnostic_codes::ALL.contains(&code.as_str()) {
                return Err(format!("{code:?} is not a known diagnostic code"));
            }
        }
    }
    if let CheckScope::Roles(roles) = check.scope()
        && (roles.is_empty() || roles.iter().any(|role| role.trim().is_empty()))
    {
        return Err("roles must name at least one non-empty role".into());
    }
    Ok(())
}

fn refused(reason: &str) -> crate::StorageError {
    crate::StorageError::Serialization(format!("evaluation sample {reason}"))
}
