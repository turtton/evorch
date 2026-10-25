use std::path::{Component, Path, PathBuf};

use sandbox::{BoundaryRegion, PathAccess, SandboxBoundary};

use super::ToolExecutor;
use crate::tools::file_access::{JUSTIFICATION, SANDBOX_ACCESS};
use crate::tools::shell_escalation::FileAccessRequest;
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
            guidance: None,
        };
        let paths: Vec<PathBuf> = match name {
            "shell" | "git_diff" => args
                .get("cwd")
                .and_then(serde_json::Value::as_str)
                .map(PathBuf::from)
                .into_iter()
                .collect(),
            _ if is_file_tool(name) => self
                .file_targets(name, args)
                .map_err(|_| deny())?
                .into_iter()
                .map(|(path, _)| path)
                .collect(),
            _ if permissions.fs_read || permissions.fs_write || permissions.process_spawn => {
                return Err(deny());
            }
            _ => return Ok(()),
        };
        // An apply_patch input that does not parse names no path; the tool
        // rejects it with the same parser before touching any file.
        if paths.is_empty() && name != apply_patch::tool_contract::NAME {
            return Err(deny());
        }
        for path in &paths {
            // Avoid resolving a missing path through lexical parent traversal.
            // Absolute paths are supported only within the frozen workspace.
            if !path.is_absolute()
                || path.components().any(|part| part == Component::ParentDir)
                || !path.starts_with(root)
                || !existing_ancestor_is_inside(path, root)?
                || (is_file_tool(name)
                    && !canonical_entry_location(path).is_some_and(|entry| entry.starts_with(root)))
                || std::fs::metadata(path)
                    .is_ok_and(|metadata| !metadata.is_file() && !metadata.is_dir())
            {
                return Err(deny());
            }
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
    ) -> Result<(), ToolError> {
        let deny = |reason: String| ToolError::ExecutionDenied {
            tool_name: name.into(),
            reason,
            guidance: None,
        };
        if name == "browser_upload_file" {
            return self.enforce_upload_boundary(args).map_err(deny);
        }
        if !is_file_tool(name) {
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
                Err(deny(
                    "benchmark file tools cannot leave the recorded workspace".into(),
                ))
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
        let targets = self.file_boundary_targets(name, args).map_err(deny)?;
        let write = targets.iter().any(|(_, write, _)| *write);
        let mut outside = Vec::new();
        let mut violation = None;
        for (path, write, entry) in targets {
            let location = if entry {
                canonical_entry_location(&path)
            } else {
                canonical_location(&path)
            };
            let placement = place(&regions, location.as_deref());
            let allowed = match placement {
                Some(PathAccess::ReadWrite) => true,
                Some(PathAccess::ReadOnly) => !write,
                None => false,
            };
            if allowed {
                continue;
            }
            let target = location.unwrap_or(path);
            if violation.is_none() {
                violation = Some((target.clone(), placement.is_some()));
            }
            if !outside.contains(&target) {
                outside.push(target);
            }
        }
        // unsandboxed only lifts the path boundary, so a call that stays
        // inside it has nothing to review and runs as if isolated.
        let Some((path, read_only)) = violation else {
            return Ok(());
        };
        if !unsandboxed {
            return Err(deny(boundary_denial(
                &path,
                read_only,
                name != apply_patch::tool_contract::NAME,
            )));
        }
        let justification = args
            .get(JUSTIFICATION)
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        if justification.trim().is_empty() {
            return Err(deny(
                "file access escalation denied: justification is required".into(),
            ));
        }
        let gate = self
            .file_escalation
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let Some(gate) = gate else {
            return Err(deny(
                "file access escalation denied: no escalation gate configured".into(),
            ));
        };
        let request = FileAccessRequest {
            tool: name.to_owned(),
            paths: outside,
            write,
            justification: justification.to_owned(),
            change: requested_change(name, args),
        };
        let decision = gate.decide_file_access(ctx, &request).await;
        if let Some((reason, guidance)) = decision.denial() {
            let reason = if reason.starts_with("file access escalation") {
                reason.to_owned()
            } else {
                format!("file access escalation denied: {reason}")
            };
            Err(ToolError::ExecutionDenied {
                tool_name: name.into(),
                reason,
                guidance: Some(guidance),
            })
        } else {
            Ok(())
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

/// Tools whose arguments name the files they read or write directly, without
/// an OS sandbox around them.
fn is_file_tool(name: &str) -> bool {
    name == apply_patch::tool_contract::NAME || !file_path_keys(name).is_empty()
}

impl ToolExecutor {
    /// Entry writes and referent accesses are distinct: remove_file unlinks the
    /// final component, whereas apply_patch writes through a final symlink.
    fn file_boundary_targets(
        &self,
        name: &str,
        args: &serde_json::Value,
    ) -> Result<Vec<(PathBuf, bool, bool)>, String> {
        let targets = self.file_targets(name, args)?;
        if name != apply_patch::tool_contract::NAME {
            return Ok(targets
                .into_iter()
                .flat_map(|(path, write)| {
                    let mut accesses = vec![(path.clone(), write, false)];
                    if write {
                        accesses.push((path, true, true));
                    }
                    accesses
                })
                .collect());
        }
        let input = args
            .get("input")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let kinds = apply_patch::targets_raw(input).unwrap_or_default();
        Ok(targets
            .into_iter()
            .zip(kinds)
            .flat_map(|((path, _), (_, kind))| {
                let writes_content = matches!(
                    kind,
                    apply_patch::TargetKind::Add
                        | apply_patch::TargetKind::Update
                        | apply_patch::TargetKind::MoveDest
                );
                [(path.clone(), true, true), (path, writes_content, false)]
            })
            .collect())
    }

    /// Every path a direct file tool names in `args`, with whether the tool
    /// writes there. Other tools name none.
    ///
    /// apply_patch writes to every path of the patch: added, updated and
    /// deleted files and both ends of a move. Relative paths are resolved
    /// against this run's workspace and normalized as
    /// [`apply_patch::targets`] does, so the boundary checks the very paths the
    /// tool writes; without a workspace they stay relative and no boundary
    /// admits them. Unlike `targets`, a `..` is kept for [`canonical_location`]
    /// to refuse. A patch that does not parse names no path, and the tool then
    /// rejects it with the same parser before touching any file.
    ///
    /// # Errors
    /// A named path that exists but is neither a regular file nor a directory,
    /// such as a FIFO a host-side read would block on.
    pub(super) fn file_targets(
        &self,
        name: &str,
        args: &serde_json::Value,
    ) -> Result<Vec<(PathBuf, bool)>, String> {
        let targets: Vec<(PathBuf, bool)> = if name == apply_patch::tool_contract::NAME {
            let input = args
                .get("input")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            let Ok(raw) = apply_patch::targets_raw(input) else {
                return Ok(Vec::new());
            };
            let root = self.default_cwd();
            raw.into_iter()
                .map(|(path, _)| {
                    let path = match &root {
                        Some(root) => root.join(path),
                        None => path,
                    };
                    (path.components().collect(), true)
                })
                .collect()
        } else {
            let write = matches!(name, "write" | "edit");
            file_path_keys(name)
                .iter()
                .filter_map(|key| args.get(*key).and_then(serde_json::Value::as_str))
                .map(|path| (PathBuf::from(path), write))
                .collect()
        };
        if let Some((path, _)) = targets.iter().find(|(path, _)| {
            std::fs::metadata(path).is_ok_and(|metadata| !metadata.is_file() && !metadata.is_dir())
        }) {
            return Err(format!(
                "{} is not a regular file or directory",
                path.display()
            ));
        }
        Ok(targets)
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

/// Resolve ancestors but not the directory entry changed by unlink/rename.
fn canonical_entry_location(path: &Path) -> Option<PathBuf> {
    if !path.is_absolute() || path.components().any(|part| part == Component::ParentDir) {
        return None;
    }
    Some(canonical_location(path.parent()?)?.join(path.file_name()?))
}

/// Like bwrap, the last mounted region covering a path decides its access.
fn place(regions: &[BoundaryRegion], location: Option<&Path>) -> Option<PathAccess> {
    sandbox::region_access(regions, location?)
}

/// Why a direct file tool may not touch `path`, and what to do about it.
/// `escalation` names the reviewed `sandbox_access` retry only for tools that
/// accept it; apply_patch does not, so its denial never suggests one.
fn boundary_denial(path: &Path, read_only: bool, escalation: bool) -> String {
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
    if !escalation {
        return format!(
            "{problem}. Nothing was changed: apply_patch only writes where the sandbox boundary lets file tools write. If the location is needed, ask the user to register it as an allowed directory of the project."
        );
    }
    format!(
        "{problem}. To access it once, retry with sandbox_access: \"unsandboxed\" and a justification for review. If the location is needed repeatedly, ask the user to register it as an allowed directory of the project."
    )
}

/// Bytes of write content, of an edit call's old/new texts together, or of an
/// apply_patch input, shown to the reviewer.
const MAX_CHANGE_BYTES: usize = 4000;

fn requested_change(name: &str, args: &serde_json::Value) -> Option<String> {
    match name {
        "write" => args
            .get("content")?
            .as_str()
            .map(|content| bounded(content, MAX_CHANGE_BYTES)),
        "edit" => edit_change(args),
        "apply_patch" => args
            .get("input")?
            .as_str()
            .map(|patch| bounded(patch, MAX_CHANGE_BYTES)),
        _ => None,
    }
}

/// What an edit approval covers. Only the old/new texts are cut, so a long
/// text can never hide a later edit, its index or whether it replaces one
/// match or all of them; the labels grow with the number of edits instead.
fn edit_change(args: &serde_json::Value) -> Option<String> {
    let (edits, indexed) = match args.get("edits").and_then(serde_json::Value::as_array) {
        Some(edits) => (edits.as_slice(), true),
        None => (std::slice::from_ref(args), false),
    };
    let mut texts = Vec::with_capacity(edits.len());
    for edit in edits {
        let text = |key: &str| edit.get(key)?.as_str();
        texts.push([text("old_string")?, text("new_string")?]);
    }
    let cap = crate::output::shared_cap(
        texts.iter().flatten().map(|text| text.len()).collect(),
        MAX_CHANGE_BYTES,
    );
    let mut parts = Vec::with_capacity(edits.len() + 1);
    if indexed {
        parts.push(format!("edits: {}", edits.len()));
    }
    for (index, (edit, [old, new])) in edits.iter().zip(&texts).enumerate() {
        let label = if indexed {
            format!("edits[{index}].")
        } else {
            String::new()
        };
        let replace_all =
            edit.get("replace_all").and_then(serde_json::Value::as_bool) == Some(true);
        parts.push(format!(
            "{label}replace_all: {replace_all}\n{label}old_string:\n{}\n{label}new_string:\n{}",
            bounded(old, cap),
            bounded(new, cap)
        ));
    }
    Some(parts.join("\n"))
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

#[cfg(test)]
mod apply_patch_tests {
    use std::sync::Arc;

    use super::*;

    fn executor(cwd: Option<&Path>) -> ToolExecutor {
        let executor = ToolExecutor::new(Arc::new(event_bus::EventBus::new(4)));
        if let Some(cwd) = cwd {
            executor.set_default_cwd(cwd.to_path_buf());
        }
        executor
    }

    // The reviewer sees a bounded patch, like a long write content.
    #[test]
    fn requested_change_bounds_the_patch_text() {
        let patch = format!("*** Begin Patch\n*** Add File: a\n+{}", "x".repeat(10_000));
        let change =
            requested_change("apply_patch", &serde_json::json!({ "input": patch })).unwrap();
        assert!(change.starts_with("*** Begin Patch\n*** Add File: a\n+x"));
        assert!(
            change.ends_with(&format!("…[{} more bytes]", patch.len() - MAX_CHANGE_BYTES)),
            "{change}"
        );
        let short = "*** Begin Patch\n*** Delete File: a\n*** End Patch";
        assert_eq!(
            requested_change("apply_patch", &serde_json::json!({ "input": short })).as_deref(),
            Some(short)
        );
    }

    // Every path is a write target, resolved and normalized like the tool's;
    // `..` survives for canonical_location to refuse.
    #[test]
    fn file_targets_list_every_patch_path_as_a_write() {
        let root = Path::new("/workspace/root");
        let patch = "*** Begin Patch\n*** Add File: ./a//b.txt\n+x\n*** Delete File: /abs/c.txt\n*** Update File: src/old.rs\n*** Move to: ../moved.rs\n@@\n-a\n+b\n*** End Patch";
        let args = serde_json::json!({ "input": patch });
        assert_eq!(
            executor(Some(root)).file_targets("apply_patch", &args),
            Ok(vec![
                (root.join("a/b.txt"), true),
                (PathBuf::from("/abs/c.txt"), true),
                (root.join("src/old.rs"), true),
                (PathBuf::from("/workspace/root/../moved.rs"), true),
            ])
        );
        let resolved: Vec<_> = apply_patch::targets(
            "*** Begin Patch\n*** Add File: ./a//b.txt\n+x\n*** End Patch",
            Some(root),
        )
        .unwrap()
        .into_iter()
        .map(|target| (target.resolved, true))
        .collect();
        assert_eq!(
            executor(Some(root)).file_targets(
                "apply_patch",
                &serde_json::json!({
                    "input": "*** Begin Patch\n*** Add File: ./a//b.txt\n+x\n*** End Patch"
                })
            ),
            Ok(resolved)
        );
        // Without a workspace a relative path stays relative, which no
        // boundary places.
        assert_eq!(
            executor(None).file_targets("apply_patch", &args).unwrap()[0],
            (PathBuf::from("./a/b.txt"), true)
        );
        for input in ["not a patch", ""] {
            assert_eq!(
                executor(Some(root))
                    .file_targets("apply_patch", &serde_json::json!({ "input": input })),
                Ok(Vec::new())
            );
        }
    }
}

#[cfg(all(test, unix))]
mod allowed_identity_compatibility_tests {
    use super::*;

    // Existing allowed directories remain live path grants in this PR. Unlike
    // thread RO roots, place() follows a replaced allowed root on every use.
    #[test]
    fn project_allowed_root_still_follows_a_replacement_symlink() {
        let temp = tempfile::tempdir().unwrap();
        let allowed = temp.path().join("allowed");
        let other = temp.path().join("other");
        std::fs::create_dir(&allowed).unwrap();
        std::fs::create_dir(&other).unwrap();
        let regions = vec![BoundaryRegion {
            path: allowed.clone(),
            access: PathAccess::ReadWrite,
        }];
        let file = other.canonicalize().unwrap().join("secret");
        assert_eq!(place(&regions, Some(&file)), None);
        std::fs::rename(&allowed, temp.path().join("old")).unwrap();
        std::os::unix::fs::symlink(&other, &allowed).unwrap();
        assert_eq!(place(&regions, Some(&file)), Some(PathAccess::ReadWrite));
    }
}
