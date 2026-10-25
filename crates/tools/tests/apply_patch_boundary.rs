//! apply_patch under the executor's path boundaries: the sandbox boundary of
//! ADR 0033 and the benchmark workspace. Every path of a patch is checked
//! before the tool runs, and apply_patch has no reviewed escape.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use event_bus::EventBus;
use sandbox::{DirectSandbox, SandboxBoundary};
use serde_json::json;
use tools::tools::shell_escalation::{EscalationDecision, FileAccessRequest, ShellEscalationGate};
use tools::{ToolError, ToolExecutionContext, ToolExecutor, ToolResult};

fn context() -> ToolExecutionContext {
    ToolExecutionContext {
        run_id: "run-1".into(),
        thread_id: None,
        call_id: None,
    }
}

fn patch(body: &str) -> String {
    format!("*** Begin Patch\n{body}\n*** End Patch")
}

struct Layout {
    _dirs: Vec<tempfile::TempDir>,
    project: PathBuf,
    scratch: PathBuf,
    readonly: PathBuf,
    outside: PathBuf,
}

fn layout() -> Layout {
    let dirs = (0..4)
        .map(|_| tempfile::tempdir().unwrap())
        .collect::<Vec<_>>();
    let path = |index: usize| dirs[index].path().canonicalize().unwrap();
    let layout = Layout {
        project: path(0),
        scratch: path(1),
        readonly: path(2),
        outside: path(3),
        _dirs: dirs,
    };
    std::fs::write(layout.project.join("inside.txt"), "project\n").unwrap();
    std::fs::write(layout.readonly.join("shared.txt"), "read-only\n").unwrap();
    std::fs::write(layout.outside.join("secret.txt"), "secret\n").unwrap();
    layout
}

impl Layout {
    fn boundary(&self) -> SandboxBoundary {
        SandboxBoundary::new(self.project.clone())
            .read_only(&self.readonly)
            .with_scratch(&self.scratch)
    }

    fn roots(&self) -> [&Path; 4] {
        [&self.project, &self.scratch, &self.readonly, &self.outside]
    }

    /// Every file and directory under the layout's roots.
    fn snapshot(&self) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
        let mut out = BTreeMap::new();
        for root in self.roots() {
            visit(root, &mut out);
        }
        out
    }
}

fn visit(dir: &Path, out: &mut BTreeMap<PathBuf, Option<Vec<u8>>>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        let metadata = std::fs::symlink_metadata(&path).unwrap();
        if metadata.is_dir() {
            out.insert(path.clone(), None);
            visit(&path, out);
        } else if metadata.is_file() {
            out.insert(path.clone(), Some(std::fs::read(&path).unwrap()));
        } else {
            out.insert(path.clone(), Some(Vec::new()));
        }
    }
}

#[derive(Default)]
struct RecordingGate(Mutex<Vec<FileAccessRequest>>);

#[async_trait::async_trait]
impl ShellEscalationGate for RecordingGate {
    async fn decide(&self, _: &ToolExecutionContext, _: &str, _: &str) -> EscalationDecision {
        EscalationDecision::Approve
    }

    async fn decide_file_access(
        &self,
        _: &ToolExecutionContext,
        request: &FileAccessRequest,
    ) -> EscalationDecision {
        self.0.lock().unwrap().push(request.clone());
        EscalationDecision::Approve
    }
}

/// A standard executor confined to `layout`, with an approving escalation
/// gate that apply_patch must never consult.
fn executor(layout: &Layout, cwd: &Path) -> (ToolExecutor, Arc<RecordingGate>) {
    let executor = ToolExecutor::with_standard_tools_in(
        Arc::new(EventBus::new(64)),
        Arc::new(DirectSandbox::new_unchecked()),
        Some(cwd.to_path_buf()),
    );
    executor.set_file_boundary(&layout.boundary());
    let gate = Arc::new(RecordingGate::default());
    executor.set_shell_escalation(gate.clone(), Arc::new(DirectSandbox::new_unchecked()));
    (executor, gate)
}

async fn apply(executor: &ToolExecutor, body: &str) -> Result<ToolResult, ToolError> {
    executor
        .execute(
            &context(),
            "apply_patch",
            "call",
            json!({ "input": patch(body) }),
        )
        .await
}

fn denial(result: Result<ToolResult, ToolError>) -> String {
    match result {
        Err(ToolError::ExecutionDenied {
            tool_name, reason, ..
        }) => {
            assert_eq!(tool_name, "apply_patch");
            reason
        }
        other => panic!("expected a denial, got {other:?}"),
    }
}

/// The denial names the path, changes nothing, offers no escalation and
/// points at the allowed directories instead.
async fn assert_denied(layout: &Layout, executor: &ToolExecutor, body: &str, path: &Path) {
    let before = layout.snapshot();
    let reason = denial(apply(executor, body).await);
    assert!(reason.starts_with(&path.display().to_string()), "{reason}");
    assert!(!reason.contains("sandbox_access"), "{reason}");
    assert!(!reason.contains("unsandboxed"), "{reason}");
    assert!(reason.contains("Nothing was changed"), "{reason}");
    assert!(
        reason.contains("ask the user to register it as an allowed directory"),
        "{reason}"
    );
    assert_eq!(layout.snapshot(), before, "{body}");
}

// Given: patches inside the project and scratch / When: applied / Then: they
// are written like any edit inside the boundary.
#[tokio::test]
async fn patches_inside_the_boundary_are_applied() {
    let layout = layout();
    let (executor, gate) = executor(&layout, &layout.project);
    let result = apply(
        &executor,
        &format!(
            "*** Update File: inside.txt\n@@\n-project\n+edited\n*** Add File: {}\n+scratch\n*** Add File: {}\n+absolute",
            layout.scratch.join("note.txt").display(),
            layout.project.join("nested/absolute.txt").display()
        ),
    )
    .await
    .unwrap();
    assert!(!result.is_error, "{}", result.content);
    assert_eq!(
        std::fs::read_to_string(layout.project.join("inside.txt")).unwrap(),
        "edited\n"
    );
    assert_eq!(
        std::fs::read_to_string(layout.scratch.join("note.txt")).unwrap(),
        "scratch\n"
    );
    assert!(layout.project.join("nested/absolute.txt").is_file());
    assert!(gate.0.lock().unwrap().is_empty());
}

// A writable referent does not grant permission to unlink its outside alias.
#[cfg(unix)]
#[tokio::test]
async fn symlink_entries_outside_writable_regions_refuse_the_whole_patch() {
    assert_symlink_entry_denied(false).await;
}

#[cfg(unix)]
#[tokio::test]
async fn symlink_move_sources_outside_writable_regions_refuse_the_whole_patch() {
    assert_symlink_entry_denied(true).await;
}

#[cfg(unix)]
async fn assert_symlink_entry_denied(move_first: bool) {
    let layout = layout();
    let (executor, gate) = executor(&layout, &layout.project);
    for root in [&layout.outside, &layout.readonly] {
        let link = root.join("link.txt");
        std::os::unix::fs::symlink(layout.project.join("inside.txt"), &link).unwrap();
        let delete = format!("*** Delete File: {}", link.display());
        let move_source = format!(
            "*** Update File: {}\n*** Move to: moved.txt\n@@\n-project\n+moved",
            link.display()
        );
        let first = if move_first { move_source } else { delete };
        for operation in [
            first,
            format!("*** Add File: {}\n+replacement", link.display()),
            format!(
                "*** Update File: {}\n@@\n-project\n+updated",
                link.display()
            ),
            format!(
                "*** Update File: inside.txt\n*** Move to: {}\n@@\n-project\n+moved",
                link.display()
            ),
        ] {
            let body = format!("*** Add File: other.txt\n+untouched\n{operation}");
            assert_denied(&layout, &executor, &body, &link).await;
        }
    }
    assert!(gate.0.lock().unwrap().is_empty());
}

#[cfg(unix)]
#[tokio::test]
async fn edit_and_write_cannot_replace_outside_symlink_entries() {
    let layout = layout();
    let (executor, _) = executor(&layout, &layout.project);
    for root in [&layout.outside, &layout.readonly] {
        let link = root.join("entry.txt");
        std::os::unix::fs::symlink(layout.project.join("inside.txt"), &link).unwrap();
        for (name, args) in [
            ("write", json!({ "path": link, "content": "replacement\n" })),
            (
                "edit",
                json!({ "path": link, "old_string": "project", "new_string": "replacement" }),
            ),
        ] {
            let before = layout.snapshot();
            let result = executor.execute(&context(), name, "call", args).await;
            assert!(
                matches!(result, Err(ToolError::ExecutionDenied { .. })),
                "{result:?}"
            );
            assert_eq!(layout.snapshot(), before);
            assert!(std::fs::symlink_metadata(&link).unwrap().is_symlink());
        }
    }
}

#[cfg(unix)]
#[tokio::test]
async fn workspace_boundary_checks_unlinked_entries_beneath_parent_symlinks() {
    let layout = layout();
    let (executor, _) = executor(&layout, &layout.project);
    executor
        .set_workspace_boundary(layout.project.clone())
        .unwrap();
    std::os::unix::fs::symlink(&layout.outside, layout.project.join("parent-link")).unwrap();
    std::os::unix::fs::symlink(
        layout.project.join("inside.txt"),
        layout.outside.join("entry.txt"),
    )
    .unwrap();
    for operation in [
        "*** Delete File: parent-link/entry.txt",
        "*** Update File: parent-link/entry.txt\n*** Move to: moved.txt\n@@\n-project\n+moved",
    ] {
        let before = layout.snapshot();
        let result = apply(
            &executor,
            &format!("*** Add File: other.txt\n+untouched\n{operation}"),
        )
        .await;
        assert!(
            matches!(result, Err(ToolError::ExecutionDenied { .. })),
            "{result:?}"
        );
        assert_eq!(layout.snapshot(), before);
    }
}

#[cfg(unix)]
#[tokio::test]
async fn writable_symlink_entries_can_be_unlinked_without_writing_readonly_referents() {
    let layout = layout();
    let (executor, _) = executor(&layout, &layout.project);
    let target = layout.readonly.join("shared.txt");
    for (name, operation) in [
        ("delete-link", "*** Delete File: delete-link"),
        (
            "move-link",
            "*** Update File: move-link\n*** Move to: moved.txt\n@@\n-read-only\n+moved",
        ),
    ] {
        let link = layout.project.join(name);
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let result = apply(&executor, operation).await.unwrap();
        assert!(!result.is_error, "{}", result.content);
        assert!(std::fs::symlink_metadata(link).is_err());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "read-only\n");
    }
    assert_eq!(
        std::fs::read_to_string(layout.project.join("moved.txt")).unwrap(),
        "moved\n"
    );
}

// Given: a patch that also edits a read-only region / When: applied / Then:
// it is refused as a whole, before its in-project changes are written.
#[tokio::test]
async fn read_only_regions_refuse_the_whole_patch() {
    let layout = layout();
    let (executor, gate) = executor(&layout, &layout.project);
    let shared = layout.readonly.join("shared.txt");
    for (body, path) in [
        (
            format!(
                "*** Add File: new.txt\n+x\n*** Update File: {}\n@@\n-read-only\n+changed",
                shared.display()
            ),
            shared.clone(),
        ),
        (
            format!("*** Delete File: {}", shared.display()),
            shared.clone(),
        ),
        (
            format!(
                "*** Add File: {}\n+x",
                layout.readonly.join("new.txt").display()
            ),
            layout.readonly.join("new.txt"),
        ),
    ] {
        assert_denied(&layout, &executor, &body, &path).await;
    }
    let reason = denial(apply(&executor, &format!("*** Delete File: {}", shared.display())).await);
    assert!(reason.contains("is read-only for file tools"), "{reason}");
    assert!(gate.0.lock().unwrap().is_empty());
}

// Given: absolute paths outside every region / When: added, updated or
// deleted / Then: the patch is refused and the files are untouched.
#[tokio::test]
async fn absolute_paths_outside_the_boundary_are_refused() {
    let layout = layout();
    let (executor, gate) = executor(&layout, &layout.project);
    let secret = layout.outside.join("secret.txt");
    for (body, path) in [
        (
            format!(
                "*** Add File: {}\n+x",
                layout.outside.join("new.txt").display()
            ),
            layout.outside.join("new.txt"),
        ),
        (
            format!(
                "*** Add File: ok.txt\n+x\n*** Update File: {}\n@@\n-secret\n+leaked",
                secret.display()
            ),
            secret.clone(),
        ),
        (
            format!("*** Delete File: {}", secret.display()),
            secret.clone(),
        ),
    ] {
        assert_denied(&layout, &executor, &body, &path).await;
    }
    let reason = denial(apply(&executor, &format!("*** Delete File: {}", secret.display())).await);
    assert!(
        reason.contains("is outside the sandbox boundary"),
        "{reason}"
    );
    assert!(gate.0.lock().unwrap().is_empty());
}

// Given: a move whose source is in the project / When: only its destination
// is outside / Then: the source is neither removed nor changed.
#[tokio::test]
async fn a_move_with_only_its_destination_outside_is_refused() {
    let layout = layout();
    let (executor, _) = executor(&layout, &layout.project);
    let destination = layout.outside.join("moved.txt");
    assert_denied(
        &layout,
        &executor,
        &format!(
            "*** Update File: inside.txt\n*** Move to: {}\n@@\n-project\n+moved",
            destination.display()
        ),
        &destination,
    )
    .await;
}

// Given: links inside the project that lead outside / When: a patch writes
// through them / Then: the link target decides, as for write and edit.
#[cfg(unix)]
#[tokio::test]
async fn symlinks_leading_outside_are_refused() {
    let layout = layout();
    let (executor, _) = executor(&layout, &layout.project);
    std::os::unix::fs::symlink(&layout.outside, layout.project.join("dir-link")).unwrap();
    std::os::unix::fs::symlink(
        layout.outside.join("secret.txt"),
        layout.project.join("file-link"),
    )
    .unwrap();
    let secret = layout.outside.join("secret.txt");
    assert_denied(
        &layout,
        &executor,
        "*** Update File: dir-link/secret.txt\n@@\n-secret\n+leaked",
        &secret,
    )
    .await;
    assert_denied(
        &layout,
        &executor,
        "*** Update File: file-link\n@@\n-secret\n+leaked",
        &secret,
    )
    .await;
    assert_denied(
        &layout,
        &executor,
        "*** Add File: dir-link/new.txt\n+x",
        &layout.outside.join("new.txt"),
    )
    .await;
}

// Given: a dangling link in the project / When: a patch adds a file through
// it / Then: it is refused rather than treated as a new in-project file.
#[cfg(unix)]
#[tokio::test]
async fn dangling_symlinks_are_refused() {
    let layout = layout();
    let (executor, _) = executor(&layout, &layout.project);
    let target = layout.outside.join("created-through-link.txt");
    std::os::unix::fs::symlink(&target, layout.project.join("dangling")).unwrap();
    assert_denied(
        &layout,
        &executor,
        "*** Add File: dangling\n+x",
        &layout.project.join("dangling"),
    )
    .await;
    assert!(!target.exists());
}

// Given: `..` in relative or absolute paths, even back into the project /
// When: applied / Then: the boundary refuses them without resolving them.
#[tokio::test]
async fn parent_dir_components_are_refused_by_the_boundary() {
    let layout = layout();
    let (executor, _) = executor(&layout, &layout.project);
    let name = layout.project.file_name().unwrap().to_string_lossy();
    for (body, path) in [
        (
            "*** Add File: ../escape.txt\n+x".to_owned(),
            layout.project.join("../escape.txt"),
        ),
        (
            format!("*** Add File: ../{name}/inside-again.txt\n+x"),
            layout.project.join(format!("../{name}/inside-again.txt")),
        ),
        (
            format!(
                "*** Update File: {}/../{name}/inside.txt\n@@\n-project\n+x",
                layout.project.display()
            ),
            layout.project.join(format!("../{name}/inside.txt")),
        ),
    ] {
        assert_denied(&layout, &executor, &body, &path).await;
    }
}

// Given: a socket in the project / When: a patch names it / Then: it is
// refused as neither a regular file nor a directory.
#[cfg(unix)]
#[tokio::test]
async fn special_files_are_refused() {
    let layout = layout();
    let (executor, _) = executor(&layout, &layout.project);
    let socket = layout.project.join("socket");
    let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let before = layout.snapshot();
    let reason = denial(apply(&executor, "*** Delete File: socket").await);
    assert_eq!(
        reason,
        format!("{} is not a regular file or directory", socket.display())
    );
    assert_eq!(layout.snapshot(), before);
}

// Given: a patch that does not parse / When: applied / Then: the boundary
// names no path and the tool rejects it with the same parser.
#[tokio::test]
async fn unparsable_patches_reach_the_tool_and_change_nothing() {
    let layout = layout();
    let (executor, _) = executor(&layout, &layout.project);
    let before = layout.snapshot();
    let input = format!(
        "*** Add File: {}\n+x",
        layout.outside.join("new.txt").display()
    );
    let result = executor
        .execute(&context(), "apply_patch", "bad", json!({ "input": input }))
        .await
        .unwrap();
    assert!(result.is_error);
    assert!(
        result
            .content
            .starts_with("apply_patch verification failed: invalid patch:"),
        "{}",
        result.content
    );
    assert_eq!(layout.snapshot(), before);
}

// Given: two executors whose workspaces differ / When: the same relative
// patch is applied / Then: each checks and writes against its own workspace.
#[tokio::test]
async fn relative_paths_follow_each_executors_workspace() {
    let first = layout();
    let second = layout();
    let (one, _) = executor(&first, &first.project);
    let (two, _) = executor(&second, &second.project);
    for (executor, text) in [(&one, "one"), (&two, "two")] {
        let result = apply(executor, &format!("*** Add File: shared.txt\n+{text}"))
            .await
            .unwrap();
        assert!(!result.is_error, "{}", result.content);
    }
    assert_eq!(
        std::fs::read_to_string(first.project.join("shared.txt")).unwrap(),
        "one\n"
    );
    assert_eq!(
        std::fs::read_to_string(second.project.join("shared.txt")).unwrap(),
        "two\n"
    );

    // Confined to the second layout but working in the first project, a
    // relative path is checked where the tool would write it.
    let (crossed, _) = executor(&second, &first.project);
    let before = first.snapshot();
    let reason = denial(apply(&crossed, "*** Add File: crossed.txt\n+x").await);
    assert!(
        reason.starts_with(&first.project.join("crossed.txt").display().to_string()),
        "{reason}"
    );
    assert_eq!(first.snapshot(), before);
}

// Given: a benchmark executor confined to its recorded workspace / When:
// patches leave it / Then: they are refused at execution and preparation.
#[tokio::test]
async fn benchmark_workspace_confines_every_patch_path() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap().join("workspace");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("input.txt"), "input\n").unwrap();
    let future = dir.path().canonicalize().unwrap().join("baseline.json");
    std::fs::write(&future, "evidence\n").unwrap();
    let executor = Arc::new(ToolExecutor::with_standard_tools_in(
        Arc::new(EventBus::new(32)),
        Arc::new(DirectSandbox::new_unchecked()),
        Some(root.clone()),
    ));
    executor.set_workspace_boundary(root.clone()).unwrap();
    let snapshot = || {
        let mut out = BTreeMap::new();
        visit(dir.path(), &mut out);
        out
    };

    let result = apply(
        &executor,
        "*** Update File: input.txt\n@@\n-input\n+edited\n*** Add File: nested/new.txt\n+x",
    )
    .await
    .unwrap();
    assert!(!result.is_error, "{}", result.content);

    let before = snapshot();
    for body in [
        format!("*** Add File: {}\n+x", dir.path().join("outside.txt").display()),
        format!("*** Delete File: {}", future.display()),
        "*** Add File: ../escape.txt\n+x".to_owned(),
        "*** Add File: ok.txt\n+x\n*** Update File: input.txt\n*** Move to: ../moved.txt\n@@\n-edited\n+moved".to_owned(),
    ] {
        let args = json!({ "input": patch(&body) });
        let error = executor
            .execute(&context(), "apply_patch", "outside", args.clone())
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ToolError::ExecutionDenied { reason, .. }
                if reason == "benchmark tool access must stay inside the recorded workspace"),
            "{error:?}"
        );
        assert!(matches!(
            executor.validate_call(context(), "apply_patch".into(), "prepared".into(), args),
            Err(ToolError::ExecutionDenied { .. })
        ));
        assert_eq!(snapshot(), before, "{body}");
    }

    let result = executor
        .execute(
            &context(),
            "apply_patch",
            "bad",
            json!({ "input": "*** Delete File: input.txt" }),
        )
        .await
        .unwrap();
    assert!(result.is_error);
    assert_eq!(snapshot(), before);
}
