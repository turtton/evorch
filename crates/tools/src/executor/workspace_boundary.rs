use std::path::{Component, Path, PathBuf};

use sandbox::{BoundaryRegion, PathAccess, SandboxBoundary};

use super::ToolExecutor;
use crate::tools::file_access::{JUSTIFICATION, SANDBOX_ACCESS};
use crate::tools::shell_escalation::{EscalationDecision, FileAccessRequest};
use crate::{Permissions, ToolError, ToolExecutionContext, ToolResult};

impl ToolExecutor {
    /// Constrain direct filesystem tools to an existing workspace directory.
    ///
    /// This is opt-in: ordinary executors retain their existing access policy.
    /// Process tools still require an OS sandbox; this guard restricts their cwd,
    /// not the commands they run. Callers must prevent concurrent untrusted
    /// processes from changing filesystem links during direct tool execution.
    pub fn set_workspace_boundary(&self, root: PathBuf) -> Result<(), ToolError> {
        let root = root.canonicalize().map_err(io_error)?;
        if !root.is_dir() {
            return Err(ToolError::InvalidArgs {
                detail: "workspace boundary must be a directory".into(),
            });
        }
        *self
            .workspace_boundary
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(root);
        Ok(())
    }

    pub(super) fn validate_workspace_args(
        &self,
        name: &str,
        args: &serde_json::Value,
        permissions: Permissions,
    ) -> Result<(), ToolError> {
        let boundary = self
            .workspace_boundary
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(root) = boundary.as_ref() else {
            return Ok(());
        };
        let deny = || ToolError::ExecutionDenied {
            tool_name: name.into(),
            reason: "benchmark tool access must stay inside the recorded workspace".into(),
        };
        let keys: &[&str] = match name {
            "read" => &["path", "file", "file_path", "filename", "target"],
            "write" | "edit" | "grep" => &["path"],
            "shell" | "git_diff" => &["cwd"],
            _ if permissions.fs_read || permissions.fs_write || permissions.process_spawn => {
                return Err(deny());
            }
            _ => return Ok(()),
        };
        let mut found = false;
        for key in keys {
            if let Some(path) = args.get(*key).and_then(serde_json::Value::as_str) {
                found = true;
                let path = Path::new(path);
                // Avoid resolving a missing path through lexical parent traversal.
                // Absolute paths are supported only within the frozen workspace.
                if !path.is_absolute()
                    || path.components().any(|part| part == Component::ParentDir)
                    || !path.starts_with(root)
                    || !existing_ancestor_is_inside(path, root)?
                    || std::fs::metadata(path)
                        .is_ok_and(|metadata| !metadata.is_file() && !metadata.is_dir())
                {
                    return Err(deny());
                }
            }
        }
        if !found {
            return Err(deny());
        }
        Ok(())
    }

    /// Publish newly produced artifacts inside this trial before any result is
    /// sent. Already published references and ordinary outputs are never changed.
    pub(super) fn isolate_output_artifact(&self, result: &mut ToolResult) -> Result<(), ToolError> {
        let boundary = self
            .workspace_boundary
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(root) = boundary.as_ref() else {
            return Ok(());
        };
        let Some(original) = result
            .detail
            .as_ref()
            .and_then(|detail| detail.get("output_artifact"))
            .and_then(|artifact| artifact.get("path"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
        else {
            return Ok(());
        };
        let source = Path::new(&original).canonicalize().map_err(io_error)?;
        let shared = crate::output::output_root()
            .map_err(io_error)?
            .canonicalize()
            .map_err(io_error)?;
        if !source.starts_with(&shared) {
            return Err(ToolError::InvalidArgs {
                detail: "output artifact is outside the tool output store".into(),
            });
        }
        let directory = root.join(".benchmark-tool-output");
        if !existing_ancestor_is_inside(&directory, root)? {
            return Err(ToolError::InvalidArgs {
                detail: "trial output directory escapes the workspace".into(),
            });
        }
        std::fs::create_dir_all(&directory).map_err(io_error)?;
        let destination = directory.join(format!("{}.log", uuid::Uuid::new_v4()));
        std::fs::copy(source, &destination).map_err(io_error)?;
        let path = destination.to_string_lossy().into_owned();
        result.content = result.content.replace(&original, &path);
        if let Some(artifact) = result
            .detail
            .as_mut()
            .and_then(|detail| detail.get_mut("output_artifact"))
            .and_then(serde_json::Value::as_object_mut)
        {
            artifact.insert("path".into(), serde_json::Value::String(path));
        }
        Ok(())
    }
}

impl ToolExecutor {
    /// Confine read / grep / write / edit to the paths `boundary` mounts for the
    /// shell (ADR 0033). Paths outside need a reviewed `sandbox_access: "unsandboxed"`.
    /// Regions are listed again on every call, so allowed directories declared later apply.
    pub fn set_file_boundary(&self, boundary: &SandboxBoundary) {
        *self
            .file_boundary
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(boundary.clone());
    }

    /// Runs before the tool, so a denial never reads or returns file content.
    /// Only the path boundary is lifted here; role capabilities and the
    /// approval policy are enforced independently of this check.
    pub(super) async fn enforce_file_boundary(
        &self,
        ctx: &ToolExecutionContext,
        name: &str,
        args: &serde_json::Value,
    ) -> Result<(), String> {
        if name == "browser_upload_file" {
            return self.enforce_upload_boundary(args);
        }
        let keys = file_path_keys(name);
        if keys.is_empty() {
            return Ok(());
        }
        let unsandboxed =
            args.get(SANDBOX_ACCESS).and_then(serde_json::Value::as_str) == Some("unsandboxed");
        if self
            .workspace_boundary
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some()
        {
            return if unsandboxed {
                Err("benchmark file tools cannot leave the recorded workspace".into())
            } else {
                Ok(())
            };
        }
        let Some(regions) = self
            .file_boundary
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .map(|boundary| boundary.regions())
        else {
            return Ok(());
        };
        let write = matches!(name, "write" | "edit");
        let mut outside = Vec::new();
        let mut violation = None;
        for path in keys
            .iter()
            .filter_map(|key| args.get(*key).and_then(serde_json::Value::as_str))
        {
            let path = Path::new(path);
            if std::fs::metadata(path)
                .is_ok_and(|metadata| !metadata.is_file() && !metadata.is_dir())
            {
                return Err(format!(
                    "{} is not a regular file or directory",
                    path.display()
                ));
            }
            let location = canonical_location(path);
            let placement = place(&regions, location.as_deref());
            let allowed = match placement {
                Some(PathAccess::ReadWrite) => true,
                Some(PathAccess::ReadOnly) => !write,
                None => false,
            };
            if allowed {
                continue;
            }
            let target = location.unwrap_or_else(|| path.to_path_buf());
            if violation.is_none() {
                violation = Some((target.clone(), placement.is_some()));
            }
            outside.push(target);
        }
        // unsandboxed only lifts the path boundary, so a call that stays
        // inside it has nothing to review and runs as if isolated.
        let Some((path, read_only)) = violation else {
            return Ok(());
        };
        if !unsandboxed {
            return Err(boundary_denial(&path, read_only));
        }
        let justification = args
            .get(JUSTIFICATION)
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        if justification.trim().is_empty() {
            return Err("file access escalation denied: justification is required".into());
        }
        let gate = self
            .file_escalation
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let Some(gate) = gate else {
            return Err("file access escalation denied: no escalation gate configured".into());
        };
        let request = FileAccessRequest {
            tool: name.to_owned(),
            paths: outside,
            write,
            justification: justification.to_owned(),
            change: requested_change(name, args),
        };
        match gate.decide_file_access(ctx, &request).await {
            EscalationDecision::Approve => Ok(()),
            EscalationDecision::Deny { reason } if reason.starts_with("file access escalation") => {
                Err(reason)
            }
            EscalationDecision::Deny { reason } => {
                Err(format!("file access escalation denied: {reason}"))
            }
        }
    }
}

impl ToolExecutor {
    /// Uploads hand host files to Chromium, which sends them where the page
    /// asks. They are limited to what the run's sandbox may read, with no
    /// reviewed escape and no upload at all from an executor without a boundary.
    pub(super) fn enforce_upload_boundary(&self, args: &serde_json::Value) -> Result<(), String> {
        let Some(regions) = self
            .file_boundary
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .map(SandboxBoundary::regions)
        else {
            return Err(
                "browser_upload_file needs the run's sandbox boundary, and this run has none"
                    .into(),
            );
        };
        let paths = args
            .get("paths")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(serde_json::Value::as_str);
        for path in paths {
            let location = canonical_location(Path::new(path));
            if place(&regions, location.as_deref()).is_none() {
                return Err(format!(
                    "{} is outside what this run's sandbox may read (the project root and this thread's $TMPDIR, plus read-only system, tool output and Cargo/rustup cache paths), so it cannot be uploaded",
                    location.as_deref().unwrap_or(Path::new(path)).display()
                ));
            }
        }
        Ok(())
    }
}

fn file_path_keys(name: &str) -> &'static [&'static str] {
    match name {
        "read" => &["path", "file", "file_path", "filename", "target"],
        "write" | "edit" | "grep" => &["path"],
        _ => &[],
    }
}

/// The canonical host path, or `None` when it cannot be placed safely: a
/// relative path, any `..`, a dangling link, or an unresolvable ancestor.
fn canonical_location(path: &Path) -> Option<PathBuf> {
    if !path.is_absolute() || path.components().any(|part| part == Component::ParentDir) {
        return None;
    }
    let mut missing = Vec::new();
    for ancestor in path.ancestors() {
        match ancestor.canonicalize() {
            Ok(mut resolved) => {
                resolved.extend(missing.iter().rev());
                return Some(resolved);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if std::fs::symlink_metadata(ancestor).is_ok() {
                    return None;
                }
                missing.push(ancestor.file_name()?.to_owned());
            }
            Err(_) => return None,
        }
    }
    None
}

/// Like bwrap, the last mounted region covering a path decides its access.
fn place(regions: &[BoundaryRegion], location: Option<&Path>) -> Option<PathAccess> {
    let location = location?;
    regions
        .iter()
        .rev()
        .find(|region| {
            region
                .path
                .canonicalize()
                .is_ok_and(|root| location.starts_with(root))
        })
        .map(|region| region.access)
}

fn boundary_denial(path: &Path, read_only: bool) -> String {
    let problem = if read_only {
        format!(
            "{} is read-only for file tools inside the sandbox boundary",
            path.display()
        )
    } else {
        format!(
            "{} is outside the sandbox boundary (the project root and this thread's $TMPDIR, plus read-only system, tool output and Cargo/rustup cache paths)",
            path.display()
        )
    };
    format!(
        "{problem}. To access it once, retry with sandbox_access: \"unsandboxed\" and a justification for review. If the location is needed repeatedly, ask the user to register it as an allowed directory of the project."
    )
}

const MAX_CHANGE_BYTES: usize = 4000;

fn requested_change(name: &str, args: &serde_json::Value) -> Option<String> {
    let text = |key: &str| args.get(key).and_then(serde_json::Value::as_str);
    match name {
        "write" => text("content").map(|content| bounded(content, MAX_CHANGE_BYTES)),
        "edit" => Some(format!(
            "old_string:\n{}\nnew_string:\n{}",
            bounded(text("old_string")?, MAX_CHANGE_BYTES / 2),
            bounded(text("new_string")?, MAX_CHANGE_BYTES / 2)
        )),
        _ => None,
    }
}

fn bounded(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…[{} more bytes]", &text[..end], text.len() - end)
}

fn existing_ancestor_is_inside(path: &Path, root: &Path) -> Result<bool, ToolError> {
    for ancestor in path.ancestors() {
        match ancestor.canonicalize() {
            Ok(resolved) => return Ok(resolved.starts_with(root)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                // A dangling link must not be treated as a new file.
                if std::fs::symlink_metadata(ancestor).is_ok() {
                    return Ok(false);
                }
            }
            Err(error) => return Err(io_error(error)),
        }
    }
    Ok(false)
}

fn io_error(error: std::io::Error) -> ToolError {
    ToolError::Io {
        detail: error.to_string(),
    }
}
