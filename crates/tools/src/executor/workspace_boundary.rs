use std::path::{Component, Path, PathBuf};

use super::ToolExecutor;
use crate::{Permissions, ToolError, ToolResult};

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
