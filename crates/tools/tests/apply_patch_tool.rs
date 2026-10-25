//! The apply_patch tool: Codex's model-facing contract over the plan/commit
//! engine of the apply-patch crate.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use event_bus::EventBus;
use sandbox::DirectSandbox;
use serde_json::json;
use tools::post_edit::{PostEditHook, PostEditInput, PostEditOutcome};
use tools::{
    ApplyPatch, BuiltinTool, Permissions, Tool, ToolError, ToolExecutionContext, ToolExecutionMode,
    ToolExecutor, ToolResult,
};

const VERIFICATION_FAILED: &str = "apply_patch verification failed: ";

fn patch(body: &str) -> String {
    format!("*** Begin Patch\n{body}\n*** End Patch")
}

fn context() -> ToolExecutionContext {
    ToolExecutionContext {
        run_id: "run-1".into(),
        thread_id: None,
        call_id: None,
    }
}

fn snapshot(root: &Path) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
    fn visit(root: &Path, dir: &Path, out: &mut BTreeMap<PathBuf, Option<Vec<u8>>>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let relative = path.strip_prefix(root).unwrap().to_path_buf();
            if path.is_dir() {
                out.insert(relative, None);
                visit(root, &path, out);
            } else {
                out.insert(relative, Some(std::fs::read(&path).unwrap()));
            }
        }
    }
    let mut out = BTreeMap::new();
    visit(root, root, &mut out);
    out
}

async fn apply(tool: &ApplyPatch, input: &str) -> ToolResult {
    tool.execute(json!({ "input": input })).await.unwrap()
}

fn error_class(result: &ToolResult) -> &str {
    result.detail.as_ref().unwrap()["apply_patch"]["error_class"]
        .as_str()
        .unwrap()
}

// Given: the tool / When: its model-facing definition is read / Then: it is
// Codex's JSON function with only `input`, an exclusive read-write built-in.
#[test]
fn definition_is_the_codex_json_function_without_escalation() {
    let tool = ApplyPatch::new();
    assert_eq!(tool.name(), "apply_patch");
    assert_eq!(
        tool.description(),
        apply_patch::tool_contract::JSON_DESCRIPTION
    );
    assert_eq!(tool.schema(), apply_patch::tool_contract::json_schema());
    assert_eq!(
        tool.schema()["properties"]
            .as_object()
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        ["input"]
    );
    assert_eq!(tool.schema()["additionalProperties"], false);
    assert_eq!(tool.permissions(), Permissions::read_write());
    assert_eq!(tool.execution_mode(), ToolExecutionMode::Exclusive);
    assert_eq!(tool.builtin_identity(), Some(BuiltinTool::ApplyPatch));
}

// Given: the standard executor / When: its specs are listed / Then: apply_patch
// is the seventh standard tool with its contract unchanged.
#[test]
fn standard_executor_registers_apply_patch_as_seventh_tool() {
    let executor = ToolExecutor::with_standard_tools(
        Arc::new(EventBus::new(16)),
        Arc::new(DirectSandbox::new_unchecked()),
    );
    let specs = executor.tool_specs();
    let mut names: Vec<_> = specs.iter().map(|spec| spec.name.as_str()).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        [
            "apply_patch",
            "edit",
            "git_diff",
            "grep",
            "read",
            "shell",
            "write"
        ]
    );
    let spec = specs
        .iter()
        .find(|spec| spec.name == "apply_patch")
        .unwrap();
    assert_eq!(spec.input_schema, apply_patch::tool_contract::json_schema());
    assert_eq!(
        spec.description,
        apply_patch::tool_contract::JSON_DESCRIPTION
    );
    assert_eq!(
        executor.tool_permissions("apply_patch"),
        Some(Permissions::read_write())
    );
    assert_eq!(
        executor.tool_execution_mode("apply_patch"),
        ToolExecutionMode::Exclusive
    );
}

// Given: one patch with every kind of change / When: applied / Then: the
// result is Codex's success summary, without a diff, and lists the targets.
#[tokio::test]
async fn success_returns_the_codex_summary_and_every_target() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("update.txt"), "keep\nold\n").unwrap();
    std::fs::write(root.join("delete.txt"), "gone\n").unwrap();
    std::fs::create_dir(root.join("src")).unwrap();
    std::fs::write(root.join("src/old.rs"), "fn old() {}\n").unwrap();
    let tool = ApplyPatch::new().with_default_cwd(root.to_path_buf());

    let result = apply(
        &tool,
        &patch(
            "*** Add File: nested/add.txt\n+added\n*** Update File: update.txt\n@@\n keep\n-old\n+new\n*** Delete File: delete.txt\n*** Update File: src/old.rs\n*** Move to: src/new.rs\n@@\n-fn old() {}\n+fn new() {}",
        ),
    )
    .await;

    assert!(!result.is_error, "{}", result.content);
    assert_eq!(
        result.content,
        "Success. Updated the following files:\nA nested/add.txt\nM update.txt\nM src/new.rs\nD delete.txt\n"
    );
    let resolved = |raw: &str| root.join(raw).to_string_lossy().into_owned();
    assert_eq!(
        result.detail,
        Some(json!({"apply_patch": {"files": [
            {"raw": "nested/add.txt", "resolved": resolved("nested/add.txt"), "kind": "add"},
            {"raw": "update.txt", "resolved": resolved("update.txt"), "kind": "update"},
            {"raw": "delete.txt", "resolved": resolved("delete.txt"), "kind": "delete"},
            {"raw": "src/old.rs", "resolved": resolved("src/old.rs"), "kind": "move_source"},
            {"raw": "src/new.rs", "resolved": resolved("src/new.rs"), "kind": "move_dest"},
        ]}}))
    );
    assert_eq!(
        snapshot(root),
        BTreeMap::from([
            (PathBuf::from("nested"), None),
            (PathBuf::from("nested/add.txt"), Some(b"added\n".to_vec())),
            (PathBuf::from("src"), None),
            (PathBuf::from("src/new.rs"), Some(b"fn new() {}\n".to_vec())),
            (PathBuf::from("update.txt"), Some(b"keep\nnew\n".to_vec())),
        ])
    );
}

// Given: patches that do not parse / When: applied / Then: the error result
// carries Codex's verification prefix over the parser's message, class
// `parse`, and no file is touched.
#[tokio::test]
async fn parse_failures_use_the_codex_verification_prefix() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("file.txt"), "old\n").unwrap();
    let tool = ApplyPatch::new().with_default_cwd(dir.path().to_path_buf());
    let before = snapshot(dir.path());

    let result = apply(&tool, "*** Add File: file.txt\n+new").await;
    assert!(result.is_error);
    assert_eq!(
        result.content,
        "apply_patch verification failed: invalid patch: The first line of the patch must be '*** Begin Patch'"
    );
    assert_eq!(error_class(&result), "parse");

    for input in [
        patch("*** Frobnicate File: file.txt"),
        patch("*** Update File: file.txt"),
        "*** Begin Patch\n*** Add File: file.txt\n+new".to_owned(),
    ] {
        let result = apply(&tool, &input).await;
        assert!(result.is_error, "{input}");
        let parser = apply_patch::parse_patch(&input).unwrap_err();
        assert_eq!(result.content, format!("{VERIFICATION_FAILED}{parser}"));
        assert_eq!(error_class(&result), "parse");
        assert_eq!(snapshot(dir.path()), before, "{input}");
    }
}

// Given: a patch with no operation / When: applied / Then: like Codex, which
// rejects it while applying rather than verifying, there is no prefix.
#[tokio::test]
async fn empty_patch_reports_no_files_modified_without_prefix() {
    let dir = tempfile::tempdir().unwrap();
    let tool = ApplyPatch::new().with_default_cwd(dir.path().to_path_buf());
    let result = apply(&tool, "*** Begin Patch\n*** End Patch").await;
    assert!(result.is_error);
    assert_eq!(result.content, "No files were modified.");
    assert_eq!(error_class(&result), "parse");
}

// Given: patches that parse but do not match their files / When: applied /
// Then: the verification prefix is kept with class `match` or `io`.
#[tokio::test]
async fn verification_failures_keep_the_prefix_and_classify_the_error() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("file.txt"), "old\n").unwrap();
    let tool = ApplyPatch::new().with_default_cwd(dir.path().to_path_buf());
    let before = snapshot(dir.path());
    for (body, class) in [
        (
            "*** Add File: new.txt\n+x\n*** Update File: file.txt\n@@\n-missing\n+new",
            "match",
        ),
        (
            "*** Add File: new.txt\n+x\n*** Update File: missing.txt\n@@\n-old\n+new",
            "io",
        ),
        ("*** Delete File: missing.txt", "io"),
    ] {
        let input = patch(body);
        let result = apply(&tool, &input).await;
        assert!(result.is_error, "{body}");
        let engine = apply_patch::plan(&input, Some(dir.path())).unwrap_err();
        assert_eq!(result.content, format!("{VERIFICATION_FAILED}{engine}"));
        assert_eq!(error_class(&result), class, "{body}");
        assert_eq!(snapshot(dir.path()), before, "{body}");
    }
}

// Given: a tool without a workspace / When: a patch names a relative path /
// Then: nothing is written, not even the absolute paths of the same patch.
#[tokio::test]
async fn relative_paths_need_a_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let absolute = dir.path().join("absolute.txt");
    let tool = ApplyPatch::new();

    let result = apply(
        &tool,
        &patch(&format!(
            "*** Add File: {}\n+x\n*** Add File: relative.txt\n+y",
            absolute.display()
        )),
    )
    .await;
    assert!(result.is_error);
    assert_eq!(
        result.content,
        "apply_patch verification failed: relative path 'relative.txt' cannot be resolved without a root directory"
    );
    assert_eq!(error_class(&result), "path");
    assert!(!absolute.exists());
    assert!(!Path::new("relative.txt").exists());

    let result = apply(
        &tool,
        &patch(&format!("*** Add File: {}\n+x", absolute.display())),
    )
    .await;
    assert!(!result.is_error, "{}", result.content);
    assert_eq!(std::fs::read_to_string(&absolute).unwrap(), "x\n");
}

// Given: a path with `..` / When: applied directly to the tool / Then: it is
// refused as a path error before any write.
#[tokio::test]
async fn parent_dir_components_are_path_errors() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    std::fs::create_dir(&root).unwrap();
    let tool = ApplyPatch::new().with_default_cwd(root.clone());
    let result = apply(
        &tool,
        &patch("*** Add File: inside.txt\n+x\n*** Add File: ../escape.txt\n+y"),
    )
    .await;
    assert!(result.is_error);
    assert_eq!(
        result.content,
        "apply_patch verification failed: path '../escape.txt' must not contain '..' components"
    );
    assert_eq!(error_class(&result), "path");
    assert_eq!(
        snapshot(dir.path()),
        BTreeMap::from([(PathBuf::from("root"), None)])
    );
}

// Given: non-string input / When: executed / Then: it is an argument error,
// the only failure that is not an error result.
#[tokio::test]
async fn missing_input_is_an_argument_error() {
    let error = ApplyPatch::new()
        .execute(json!({ "input": 1 }))
        .await
        .unwrap_err();
    assert!(matches!(error, ToolError::InvalidArgs { .. }), "{error:?}");
}

// D2: unlike upstream, which applies hunks one by one, a failure found while
// planning writes nothing, and only a failure while writing leaves the files
// already written, which the result lists.
#[tokio::test]
async fn d2_only_commit_failures_leave_a_partially_applied_patch() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let tool = ApplyPatch::new().with_default_cwd(root.to_path_buf());

    // Upstream scenario 015 keeps `created.txt`; planning rejects it here.
    let result = apply(
        &tool,
        &patch("*** Add File: created.txt\n+hello\n*** Update File: missing.txt\n@@\n-old\n+new"),
    )
    .await;
    assert!(result.is_error);
    assert!(result.content.starts_with(VERIFICATION_FAILED));
    assert!(snapshot(root).is_empty());

    // `x` is written first, so `x/y.txt` can no longer be created under it.
    let result = apply(
        &tool,
        &patch("*** Add File: x\n+written\n*** Add File: x/y.txt\n+blocked"),
    )
    .await;
    assert!(result.is_error);
    assert!(
        result.content.starts_with(&format!(
            "Failed to write file {}:",
            root.join("x/y.txt").display()
        )),
        "{}",
        result.content
    );
    assert!(
        result
            .content
            .ends_with("\nApplied before the failure:\nA x\n"),
        "{}",
        result.content
    );
    assert!(!result.content.starts_with(VERIFICATION_FAILED));
    assert_eq!(error_class(&result), "io");
    assert_eq!(
        result.detail.as_ref().unwrap()["apply_patch"]["committed"],
        json!([{"raw": "x", "path": root.join("x").to_string_lossy(), "change": "added"}])
    );
    assert_eq!(
        snapshot(root),
        BTreeMap::from([(PathBuf::from("x"), Some(b"written\n".to_vec()))])
    );
}

struct PassHook;
#[async_trait]
impl PostEditHook for PassHook {
    async fn check(&self, _: &PostEditInput) -> PostEditOutcome {
        PostEditOutcome::Pass
    }
}

// Given: a standard executor with a workspace / When: a hook is attached,
// which replaces the tool, and the workspace later changes / Then: relative
// patch paths always follow the executor's current workspace.
#[tokio::test]
async fn executor_keeps_the_workspace_across_hook_refresh_and_cwd_changes() {
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    let executor = ToolExecutor::with_standard_tools_in(
        Arc::new(EventBus::new(16)),
        Arc::new(DirectSandbox::new_unchecked()),
        Some(first.path().to_path_buf()),
    )
    .with_post_edit_hook(Some(Arc::new(PassHook)));
    let add = |name: &str| json!({ "input": patch(&format!("*** Add File: {name}\n+x")) });

    let result = executor
        .execute(&context(), "apply_patch", "first", add("first.txt"))
        .await
        .unwrap();
    assert!(!result.is_error, "{}", result.content);
    assert!(first.path().join("first.txt").is_file());

    executor.set_default_cwd(second.path().to_path_buf());
    let executor = executor.with_post_edit_hook(None);
    let result = executor
        .execute(&context(), "apply_patch", "second", add("second.txt"))
        .await
        .unwrap();
    assert!(!result.is_error, "{}", result.content);
    assert!(second.path().join("second.txt").is_file());
    assert!(!first.path().join("second.txt").exists());
}

// Given: the executor / When: apply_patch is called with an escalation
// argument / Then: the schema rejects it before any file is written.
#[tokio::test]
async fn executor_rejects_sandbox_access_arguments() {
    let dir = tempfile::tempdir().unwrap();
    let executor = ToolExecutor::with_standard_tools_in(
        Arc::new(EventBus::new(16)),
        Arc::new(DirectSandbox::new_unchecked()),
        Some(dir.path().to_path_buf()),
    );
    let error = executor
        .execute(
            &context(),
            "apply_patch",
            "escalate",
            json!({
                "input": patch("*** Add File: new.txt\n+x"),
                "sandbox_access": "unsandboxed",
                "justification": "needed",
            }),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, ToolError::InvalidArgs { .. }), "{error:?}");
    assert!(snapshot(dir.path()).is_empty());
}
