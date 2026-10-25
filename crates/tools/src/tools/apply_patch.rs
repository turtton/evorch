//! Codex-compatible `apply_patch`: edits files from one patch in the format
//! models were trained on.
//!
//! The patch is parsed and every change is checked against the files before
//! any is written, so a patch that does not parse, names a path that cannot be
//! resolved, or does not match its files leaves them untouched. Writing is not
//! a transaction across files: an I/O failure while committing lists the
//! changes already written. Model-facing messages follow Codex: failures found
//! before writing start with `apply_patch verification failed: `, as the Codex
//! handler words them, while failures while writing and an empty patch are
//! reported as the patch application itself reports them.

use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use apply_patch::{
    AppliedChange, ApplyPatchError, ChangeKind, ErrorClass, PatchTarget, TargetKind, TargetsError,
};
use serde_json::json;

use super::edit::required_str;
use crate::post_edit::{PostEditHook, PostEditInput, WarningPlacement, apply_post_edit_many};
use crate::tool::BuiltinTool;
use crate::{Permissions, Tool, ToolError, ToolExecutionMode, ToolResult};

/// Where comment-checker warnings about a patch go: after Codex's success
/// message, so the model reads them with it.
const POST_EDIT_WARNING_PLACEMENT: WarningPlacement = WarningPlacement::AppendToContent;

/// The prefix the Codex handler puts on failures found before any file is
/// written.
const VERIFICATION_FAILED: &str = "apply_patch verification failed: ";

/// Applies a Codex patch. Relative paths resolve against the run's workspace
/// set through [`Tool::set_default_cwd`], never against the process directory;
/// without a workspace they are rejected.
#[derive(Default)]
pub struct ApplyPatch {
    default_cwd: RwLock<Option<PathBuf>>,
    hook: Option<Arc<dyn PostEditHook>>,
}

impl std::fmt::Debug for ApplyPatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApplyPatch")
            .field("default_cwd", &self.default_cwd())
            .field("post_edit", &self.hook.is_some())
            .finish()
    }
}

impl ApplyPatch {
    pub fn new() -> Self {
        Self::default()
    }

    /// Resolve relative patch paths against `cwd`.
    pub fn with_default_cwd(self, cwd: PathBuf) -> Self {
        self.set_cwd(cwd);
        self
    }

    /// Check the changed files once the whole patch has been written.
    pub fn with_post_edit_hook(mut self, hook: Arc<dyn PostEditHook>) -> Self {
        self.hook = Some(hook);
        self
    }

    fn default_cwd(&self) -> Option<PathBuf> {
        self.default_cwd
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn set_cwd(&self, cwd: PathBuf) {
        *self
            .default_cwd
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(cwd);
    }
}

#[async_trait::async_trait]
impl Tool for ApplyPatch {
    fn name(&self) -> &'static str {
        apply_patch::tool_contract::NAME
    }

    fn description(&self) -> &str {
        apply_patch::tool_contract::JSON_DESCRIPTION
    }

    fn builtin_identity(&self) -> Option<BuiltinTool> {
        Some(BuiltinTool::ApplyPatch)
    }

    fn schema(&self) -> serde_json::Value {
        apply_patch::tool_contract::json_schema()
    }

    /// The checker process a hook may start is authorized by the executor
    /// separately; editing permission does not cover it.
    fn permissions(&self) -> Permissions {
        Permissions::read_write()
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Exclusive
    }

    fn set_default_cwd(&self, cwd: PathBuf) {
        self.set_cwd(cwd);
    }

    /// Patch problems are returned as error results for the model, never as
    /// [`ToolError`]; `detail.apply_patch.error_class` classifies them.
    ///
    /// # Errors
    /// [`ToolError::InvalidArgs`] when `input` is not a string.
    async fn execute(&self, args: serde_json::Value) -> Result<ToolResult, ToolError> {
        let patch = required_str(&args, "input")?;
        let root = self.default_cwd();
        // The executor's file boundary skips a patch that does not parse, so the
        // same parser must reject it here before any file is read or written.
        // Paths with `..` and relative paths without a workspace end here too.
        let targets = match apply_patch::targets(patch, root.as_deref()) {
            Ok(targets) => targets,
            Err(error) => {
                let class = match error {
                    TargetsError::Parse(_) => ErrorClass::Parse,
                    TargetsError::RelativeWithoutRoot(_) | TargetsError::ParentDir(_) => {
                        ErrorClass::Path
                    }
                };
                return Ok(failure(format!("{VERIFICATION_FAILED}{error}"), class));
            }
        };
        let plan = match apply_patch::plan(patch, root.as_deref()) {
            Ok(plan) => plan,
            Err(error) => return Ok(failure(plan_failure_message(&error), error.class())),
        };
        let applied = match plan.commit() {
            Ok(applied) => applied,
            Err(error) => {
                let mut result = failure(error.to_string(), error.class());
                if let Some(detail) = result.detail.as_mut() {
                    detail["apply_patch"]["committed"] =
                        error.committed.iter().map(committed_json).collect();
                }
                return Ok(result);
            }
        };
        let mut result = ToolResult::success(applied.summary).with_detail(json!({
            "apply_patch": {"files": targets.iter().map(target_json).collect::<Vec<_>>()}
        }));
        if let Some(hook) = &self.hook {
            let inputs: Vec<_> = applied.changes.iter().filter_map(post_edit_input).collect();
            apply_post_edit_many(
                hook.as_ref(),
                &inputs,
                &mut result,
                POST_EDIT_WARNING_PLACEMENT,
            )
            .await;
        }
        Ok(result)
    }
}

/// Codex reports an empty patch from the application step, without the
/// verification prefix; every other planning failure is a verification one.
fn plan_failure_message(error: &ApplyPatchError) -> String {
    match error {
        ApplyPatchError::NoFilesModified => error.to_string(),
        _ => format!("{VERIFICATION_FAILED}{error}"),
    }
}

fn failure(message: String, class: ErrorClass) -> ToolResult {
    ToolResult::error(message).with_detail(json!({
        "apply_patch": {"error_class": class_name(class)}
    }))
}

fn class_name(class: ErrorClass) -> &'static str {
    match class {
        ErrorClass::Parse => "parse",
        ErrorClass::Match => "match",
        ErrorClass::Io => "io",
        ErrorClass::Path => "path",
    }
}

fn target_json(target: &PatchTarget) -> serde_json::Value {
    let kind = match target.kind {
        TargetKind::Add => "add",
        TargetKind::Update => "update",
        TargetKind::Delete => "delete",
        TargetKind::MoveSource => "move_source",
        TargetKind::MoveDest => "move_dest",
    };
    json!({
        "raw": target.raw.to_string_lossy(),
        "resolved": target.resolved.to_string_lossy(),
        "kind": kind,
    })
}

fn committed_json(change: &AppliedChange) -> serde_json::Value {
    let mut value = json!({
        "raw": change.raw.to_string_lossy(),
        "path": change.path.to_string_lossy(),
    });
    let kind = match &change.change {
        ChangeKind::Added { .. } => "added",
        ChangeKind::Updated { .. } => "updated",
        ChangeKind::Moved { from, .. } => {
            value["from"] = from.to_string_lossy().into();
            "moved"
        }
        ChangeKind::Deleted { .. } => "deleted",
    };
    value["change"] = kind.into();
    value
}

/// The checker input for one written file, following the Write tool: a new
/// file is checked whole, while replaced contents are an upstream `Edit` of
/// the old text so unchanged old comments are not reported. A moved file is
/// checked at its destination. Deletions and unchanged contents are skipped.
fn post_edit_input(change: &AppliedChange) -> Option<PostEditInput> {
    let (old, new) = match &change.change {
        ChangeKind::Added {
            content,
            overwritten: None,
        } => {
            return Some(PostEditInput {
                tool_name: "Write",
                file_path: change.path.clone(),
                content: Some(content.clone()),
                old_string: None,
                new_string: None,
                edits: None,
            });
        }
        ChangeKind::Added {
            content,
            overwritten: Some(old),
        } => (old, content),
        ChangeKind::Updated { old, new } | ChangeKind::Moved { old, new, .. } => (old, new),
        ChangeKind::Deleted { .. } => return None,
    };
    (old != new).then(|| PostEditInput {
        tool_name: "Edit",
        file_path: change.path.clone(),
        content: None,
        old_string: Some(old.clone()),
        new_string: Some(new.clone()),
        edits: None,
    })
}
