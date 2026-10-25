use super::LoopState;
use crate::ownership::Lease;
use crate::team::TeamBoard;
use apply_patch::TargetsError;
use providers::ToolSpec;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use tools::ToolResult;

#[cfg(test)]
#[path = "team/image_tests.rs"]
mod image_tests;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ClaimArgs {
    task_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CompleteArgs {
    task_id: String,
    generation: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FindingArgs {
    task_id: String,
    content: String,
    evidence: String,
}

impl LoopState {
    pub(super) fn team_worker(&self) -> bool {
        self.task.config.purpose == crate::RunPurpose::General
            && self.task.role == agents::Role::Worker
            && self.task.config.topology.worker_limit().is_some()
            && self.task.config.team.is_some()
    }

    pub(super) fn add_team_tools(&mut self) {
        if self.task.role == agents::Role::Orchestrator && self.task.config.team.is_some() {
            for spec in &mut self.tool_specs {
                if spec.name == "delegate" {
                    spec.description.push_str(" In team mode use target={\"role\":\"worker\"} with an optional worker category, and task={id: unique task id, paths: relative artifact paths}. At most three workers may run concurrently. Workers must claim the task before editing and complete it afterward. No automatic merge.");
                }
            }
        }
        if self.team_worker() {
            for (name, schema) in [
                (
                    "task_claim",
                    serde_json::json!({"type":"object","properties":{"task_id":{"type":"string"}},"required":["task_id"],"additionalProperties":false}),
                ),
                (
                    "task_complete",
                    serde_json::json!({"type":"object","properties":{"task_id":{"type":"string"},"generation":{"type":"integer"}},"required":["task_id","generation"],"additionalProperties":false}),
                ),
                (
                    "finding_append",
                    serde_json::json!({"type":"object","properties":{"task_id":{"type":"string"},"content":{"type":"string"},"evidence":{"type":"string"}},"required":["task_id","content","evidence"],"additionalProperties":false}),
                ),
            ] {
                self.tool_specs.push(ToolSpec {
                    name: name.into(),
                    description: name.into(),
                    input_schema: schema,
                });
            }
        }
    }

    pub(super) async fn team_tool(&self, name: &str, input: serde_json::Value) -> ToolResult {
        match self.dispatch_team_tool(name, input).await {
            Ok(text) => ToolResult::success(text),
            Err(error) => ToolResult::error(error),
        }
    }

    async fn dispatch_team_tool(
        &self,
        name: &str,
        input: serde_json::Value,
    ) -> Result<String, String> {
        if !self.team_worker() {
            return Err("team overlay is disabled".into());
        }
        let team = self.task.config.team.as_ref().ok_or("team is missing")?;
        let worker = self.task.run_id.to_string();
        match name {
            "task_claim" => {
                let args: ClaimArgs = serde_json::from_value(input).map_err(|e| e.to_string())?;
                let lease = team
                    .board
                    .claim(&args.task_id, &worker, team.now())
                    .map_err(|e| e.to_string())?;
                serde_json::to_string(&lease).map_err(|e| e.to_string())
            }
            "task_complete" => {
                let args: CompleteArgs =
                    serde_json::from_value(input).map_err(|e| e.to_string())?;
                let token = Lease {
                    owner_id: worker,
                    generation: args.generation,
                    expires_at: 0,
                };
                team.board
                    .complete(&args.task_id, &token, team.now())
                    .map_err(|e| e.to_string())?;
                Ok("completed".into())
            }
            "finding_append" => {
                let args: FindingArgs = serde_json::from_value(input).map_err(|e| e.to_string())?;
                let owns_task = team.board.snapshot().map_err(|e| e.to_string())?.iter().any(|task| {
                    task.spec.id == args.task_id && matches!(&task.state,
                        crate::team::ClaimState::Claimed(lease) if lease.owner_id == worker && lease.expires_at > team.now())
                });
                if !owns_task {
                    return Err("finding requires a live task claim".into());
                }
                let writer = team
                    .writer
                    .clone()
                    .ok_or("finding storage is not configured")?;
                let finding = storage::memory::Lesson {
                    id: format!("{}:{}", worker, args.task_id),
                    project: team.id.clone(),
                    task_id: args.task_id,
                    content: args.content,
                    evidence: args.evidence,
                    scope: storage::memory::LessonScope::Project,
                };
                tokio::task::spawn_blocking(move || writer.append_finding(&finding))
                    .await
                    .map_err(|e| e.to_string())?
                    .map_err(|e| e.to_string())?;
                Ok("appended".into())
            }
            _ => Err("unknown team tool".into()),
        }
    }

    pub(crate) fn guard_team_artifact(
        &self,
        name: &str,
        input: &serde_json::Value,
    ) -> Result<(), String> {
        if !self.team_worker() {
            return Ok(());
        }
        let team = self.task.config.team.as_ref().ok_or("team is missing")?;
        match name {
            "shell" => {
                Err("team workers use owned write/edit paths, not unrestricted shell writes".into())
            }
            // The executor resolves relative file and patch paths against its
            // workspace, so ownership is judged against the same root.
            "edit" | "write" => authorize_file(
                &team.board,
                &self.task.run_id.to_string(),
                self.shared.executor.default_cwd().as_deref(),
                input,
                team.now(),
            ),
            "apply_patch" => authorize_patch(
                &team.board,
                &self.task.run_id.to_string(),
                self.shared.executor.default_cwd().as_deref(),
                input,
                team.now(),
            ),
            "generate_image" => crate::image_generation::guard_team_claims(self, input),
            _ => Ok(()),
        }
    }
}

/// Admits an edit or write call only when the worker's live claims own its
/// path. A relative path resolves against `root`, as `ToolExecutor` scopes it
/// before the tool runs; an absolute one must lie below `root`. A root that
/// [`judged_root`] refuses rejects the call.
fn authorize_file(
    board: &TeamBoard,
    worker: &str,
    root: Option<&Path>,
    input: &serde_json::Value,
    now: u64,
) -> Result<(), String> {
    let raw = input
        .get("path")
        .and_then(serde_json::Value::as_str)
        .map(Path::new)
        .ok_or("write/edit path missing")?;
    let denied = |reason: String| {
        format!("team workers may only write or edit paths their claimed task owns: {reason}")
    };
    let root = judged_root(root).map_err(denied)?;
    let resolved = if raw.is_relative() {
        without_dots(&root.join(raw))
    } else {
        without_dots(raw)
    };
    authorize_below_root(board, worker, &root, raw, &resolved, now).map_err(denied)
}

/// Admits an apply_patch call only when the worker's live claims own every
/// path it touches, move sources and destinations and deletions included. One
/// path outside `root` or outside the claims rejects the whole call before any
/// file changes. A patch that does not parse is left to the tool, which rejects
/// it with Codex's wording without touching any file.
fn authorize_patch(
    board: &TeamBoard,
    worker: &str,
    root: Option<&Path>,
    input: &serde_json::Value,
    now: u64,
) -> Result<(), String> {
    let patch = input
        .get("input")
        .and_then(serde_json::Value::as_str)
        .ok_or("apply_patch input missing")?;
    let denied = |reason: String| {
        format!(
            "team workers may only patch paths their claimed task owns; nothing was changed: {reason}"
        )
    };
    let targets = match apply_patch::targets(patch, root) {
        Ok(targets) => targets,
        Err(TargetsError::Parse(_)) => return Ok(()),
        Err(error) => return Err(denied(error.to_string())),
    };
    // `targets` rebuilds each resolved path the same way.
    let root = judged_root(root).map_err(denied)?;
    for target in targets {
        authorize_below_root(board, worker, &root, &target.raw, &target.resolved, now)
            .map_err(denied)?;
    }
    Ok(())
}

/// Admits one path a file tool touches. `resolved` is where the tool writes,
/// rebuilt by [`without_dots`] like `root`, so a lexical prefix check tells
/// whether it lies below `root`. Symlinks are looked for from `root` down to
/// the file, never below the process working directory. Errors name the path
/// as the model wrote it.
fn authorize_below_root(
    board: &TeamBoard,
    worker: &str,
    root: &Path,
    raw: &Path,
    resolved: &Path,
    now: u64,
) -> Result<(), String> {
    let Ok(relative) = resolved.strip_prefix(root) else {
        return Err(format!("{} is outside the workspace", raw.display()));
    };
    board
        .authorize_path_under(worker, root, relative, now)
        .map_err(|error| format!("{}: {error}", raw.display()))
}

/// The workspace root ownership is judged against, rebuilt by
/// [`without_dots`]. Without one the guard cannot tell where a tool writes. A
/// relative root is refused as well: the executor scopes arguments against it
/// once when validating a call and again when running it, so each pass would
/// join it anew and the tool would write somewhere the guard never looked.
/// `ToolExecutor` makes its root absolute, so this only holds back a root that
/// could not be made absolute.
fn judged_root(root: Option<&Path>) -> Result<PathBuf, String> {
    match root {
        None => Err("the workspace root is unknown".into()),
        Some(root) if !root.is_absolute() => Err(format!(
            "the workspace root {} is not absolute",
            root.display()
        )),
        Some(root) => Ok(without_dots(root)),
    }
}

/// `path` rebuilt from its components, which drops `.` components and repeated
/// separators but keeps `..` for the ownership check to reject.
fn without_dots(path: &Path) -> PathBuf {
    path.components().collect()
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;

    use providers::{ContentBlock, ToolResultContent};
    use serde_json::{Value, json};

    use super::{authorize_file, authorize_patch};
    use crate::team::{TaskSpec, TeamBoard};

    const WORKER: &str = "worker-1";
    const NOW: u64 = 100;

    /// A board on which `WORKER` holds a live claim on `owned` and another
    /// worker holds `theirs`; `free` belongs to an unclaimed task.
    fn board() -> TeamBoard {
        let board = TeamBoard::default();
        for (id, path) in [("mine", "owned"), ("theirs", "theirs"), ("free", "free")] {
            board
                .enqueue(TaskSpec {
                    id: id.into(),
                    paths: vec![path.into()],
                })
                .unwrap();
        }
        board.claim("mine", WORKER, 0).unwrap();
        board.claim("theirs", "worker-2", 0).unwrap();
        board
    }

    fn patch(body: &str) -> Value {
        json!({ "input": format!("*** Begin Patch\n{body}\n*** End Patch") })
    }

    fn add(path: &str) -> String {
        format!("*** Add File: {path}\n+x")
    }

    fn check(root: Option<&Path>, input: &Value) -> Result<(), String> {
        authorize_patch(&board(), WORKER, root, input, NOW)
    }

    // Given: a workspace and a patch that adds, updates, deletes and moves within
    //        the worker's owned directory, written relative or absolute
    // When: the team guard checks it
    // Then: it is admitted
    #[test]
    fn patches_inside_owned_paths_are_admitted() {
        let workspace = tempfile::tempdir().unwrap();
        let root = workspace.path();
        let absolute = root.join("owned/absolute.rs");
        for input in [
            patch(&add("owned/a.rs")),
            patch(&format!(
                "{}\n*** Update File: ./owned/b.rs\n@@\n-a\n+b\n*** Delete File: owned/c.rs",
                add("owned/nested/a.rs")
            )),
            patch("*** Update File: owned/old.rs\n*** Move to: owned/new.rs\n@@\n-a\n+b"),
            patch(&add(&absolute.to_string_lossy())),
        ] {
            assert_eq!(check(Some(root), &input), Ok(()), "{input}");
        }
        // A root spelled with `.` components still matches the resolved paths.
        assert_eq!(
            check(
                Some(&root.join(".")),
                &patch(&add(&absolute.to_string_lossy()))
            ),
            Ok(())
        );
    }

    // Given: patches that each touch one path the worker does not own, next to
    //        an owned path: another worker's, an unclaimed task's, a path no task
    //        names, a move destination, a move source and a deletion
    // When: the team guard checks them
    // Then: the whole call is rejected and the reason names the offending path
    #[test]
    fn one_unowned_path_rejects_the_whole_patch() {
        let workspace = tempfile::tempdir().unwrap();
        let root = workspace.path();
        for (input, offending) in [
            (
                patch(&format!("{}\n{}", add("owned/a.rs"), add("theirs/a.rs"))),
                "theirs/a.rs",
            ),
            (
                patch(&format!("{}\n{}", add("owned/a.rs"), add("free/a.rs"))),
                "free/a.rs",
            ),
            (patch(&add("unlisted.rs")), "unlisted.rs"),
            (
                patch("*** Update File: owned/a.rs\n*** Move to: theirs/a.rs\n@@\n-a\n+b"),
                "theirs/a.rs",
            ),
            (
                patch("*** Update File: free/a.rs\n*** Move to: owned/a.rs\n@@\n-a\n+b"),
                "free/a.rs",
            ),
            (
                patch(&format!(
                    "{}\n*** Delete File: theirs/a.rs",
                    add("owned/a.rs")
                )),
                "theirs/a.rs",
            ),
        ] {
            let error = check(Some(root), &input).expect_err(offending);
            assert!(error.contains("nothing was changed"), "{error}");
            assert!(error.contains(offending), "{error}");
        }
        assert!(!root.join("owned/a.rs").exists());
    }

    // Given: patches naming a path outside the workspace, the workspace itself,
    //        or a parent-directory escape
    // When: the team guard checks them
    // Then: each is rejected
    #[test]
    fn paths_outside_the_workspace_are_rejected() {
        let workspace = tempfile::tempdir().unwrap();
        let root = workspace.path();
        let elsewhere = tempfile::tempdir().unwrap();
        for input in [
            patch(&add(&elsewhere.path().join("owned/a.rs").to_string_lossy())),
            patch(&add(&root.to_string_lossy())),
            patch(&add("../owned/a.rs")),
            patch(&format!(
                "{}\n{}",
                add("owned/a.rs"),
                add("owned/../theirs/a.rs")
            )),
        ] {
            let error = check(Some(root), &input).expect_err("outside");
            assert!(error.contains("nothing was changed"), "{error}");
        }
    }

    // Given: no workspace root
    // When: the team guard checks relative and absolute patches
    // Then: both are rejected, because ownership cannot be judged
    #[test]
    fn patches_without_a_workspace_root_are_rejected() {
        let workspace = tempfile::tempdir().unwrap();
        for input in [
            patch(&add("owned/a.rs")),
            patch(&add(&workspace.path().join("owned/a.rs").to_string_lossy())),
        ] {
            assert!(check(None, &input).is_err(), "{input}");
        }
    }

    // Given: inputs that are not a parseable patch
    // When: the team guard checks them, with or without a root
    // Then: they pass, so the tool reports the parse failure in Codex's words;
    //       a missing input is rejected like a missing edit path
    #[test]
    fn unparseable_patches_are_left_to_the_tool() {
        let workspace = tempfile::tempdir().unwrap();
        for input in [
            json!({ "input": "garbage" }),
            json!({ "input": "*** Begin Patch\n*** Add File: theirs/a.rs\n+x" }),
        ] {
            assert_eq!(check(Some(workspace.path()), &input), Ok(()), "{input}");
            assert_eq!(check(None, &input), Ok(()), "{input}");
        }
        assert!(check(Some(workspace.path()), &json!({})).is_err());
    }

    // Given: an owned directory holding a symlink to another worker's directory
    // When: a patch writes through the symlink
    // Then: it is rejected where the file lives, below the workspace root
    #[cfg(unix)]
    #[test]
    fn symlinks_below_the_workspace_root_are_rejected() {
        let workspace = tempfile::tempdir().unwrap();
        let root = workspace.path();
        std::fs::create_dir_all(root.join("owned")).unwrap();
        std::fs::create_dir_all(root.join("theirs")).unwrap();
        std::os::unix::fs::symlink(root.join("theirs"), root.join("owned/link")).unwrap();

        assert!(check(Some(root), &patch(&add("owned/link/a.rs"))).is_err());
        assert_eq!(check(Some(root), &patch(&add("owned/real/a.rs"))), Ok(()));
    }

    // Given: a claim that has expired
    // When: a patch touches its paths
    // Then: it is rejected
    #[test]
    fn expired_claims_do_not_admit_patches() {
        let workspace = tempfile::tempdir().unwrap();
        assert!(
            authorize_patch(
                &board(),
                WORKER,
                Some(workspace.path()),
                &patch(&add("owned/a.rs")),
                10_000,
            )
            .is_err()
        );
    }

    // Given: a workspace root that is still relative, next to an absolute one
    // When: the team guard checks the same owned edit/write and patch under each
    // Then: only the relative root rejects them, because it names another
    //       place every time a tool joins it again
    #[test]
    fn relative_workspace_roots_are_rejected() {
        let workspace = tempfile::tempdir().unwrap();
        let file = json!({ "path": "owned/a.rs" });
        let added = patch(&add("owned/a.rs"));
        for (root, admitted) in [
            (Path::new("relative-root"), false),
            (workspace.path(), true),
        ] {
            assert_eq!(
                authorize_file(&board(), WORKER, Some(root), &file, NOW).is_ok(),
                admitted,
                "{}",
                root.display()
            );
            assert_eq!(
                check(Some(root), &added).is_ok(),
                admitted,
                "{}",
                root.display()
            );
        }
    }

    /// A time the board treats as live for every claim taken at it, whatever the
    /// wall clock reads while a test runs, since `TeamContext::now` is real time.
    const FAR_FUTURE: u64 = u64::MAX / 2;

    /// A team Worker run whose executor resolves tool paths below `root`, with
    /// [`board`]'s tasks except that the run itself owns `owned`. `root` is a
    /// temporary directory in every caller, never the process working directory.
    fn team_worker(
        root: Option<&Path>,
    ) -> (
        tokio::sync::watch::Sender<crate::RunInterrupt>,
        crate::AgentRuntime,
        super::LoopState,
    ) {
        let (cancel, cancel_rx) = tokio::sync::watch::channel(crate::RunInterrupt::None);
        let (runtime, mut state) = super::super::delegate_cleanup_tests::fixture(cancel_rx);
        let executor = tools::ToolExecutor::with_standard_tools(
            Arc::clone(&state.shared.bus),
            Arc::new(sandbox::DirectSandbox::new_unchecked()),
        )
        .with_policy(sandbox::ApprovalPolicy::allow_all());
        if let Some(root) = root {
            executor.set_default_cwd(root.to_path_buf());
        }
        state.shared.executor = Arc::new(executor);
        state.task.role = agents::Role::Worker;
        state.policy = crate::ExecutionPolicy::for_role(agents::Role::Worker);
        state.task.config.topology = crate::CoordinationTopology::DynamicTeam { max_workers: 3 };
        let team = crate::team_context::TeamContext::new(state.task.run_id, 3);
        let worker = state.task.run_id.to_string();
        for (id, path, owner) in [
            ("mine", "owned", Some(worker.as_str())),
            ("theirs", "theirs", Some("worker-2")),
            ("free", "free", None),
        ] {
            team.board
                .enqueue(TaskSpec {
                    id: id.into(),
                    paths: vec![path.into()],
                })
                .unwrap();
            if let Some(owner) = owner {
                team.board.claim(id, owner, FAR_FUTURE).unwrap();
            }
        }
        state.task.config.team = Some(team);
        assert!(state.team_worker());
        (cancel, runtime, state)
    }

    /// What the team guard decides for `edit` and for `write` naming `path`.
    fn guard_file(state: &super::LoopState, path: &str) -> [Result<(), String>; 2] {
        ["edit", "write"].map(|name| state.guard_team_artifact(name, &json!({ "path": path })))
    }

    fn display(path: &Path) -> String {
        path.to_string_lossy().into_owned()
    }

    // Given: a team Worker whose workspace root is a temporary directory
    // When: edit or write names an owned path relative, `./`-prefixed or
    //       absolute, including under a root spelled with `.`
    // Then: the guard admits it
    #[tokio::test]
    async fn edit_and_write_inside_owned_paths_are_admitted() {
        let workspace = tempfile::tempdir().unwrap();
        let root = workspace.path();
        let (_cancel, _runtime, state) = team_worker(Some(root));
        for path in [
            "owned/a.rs".to_string(),
            "./owned/nested/a.rs".into(),
            display(&root.join("owned/a.rs")),
            display(&root.join("./owned/b.rs")),
        ] {
            assert_eq!(guard_file(&state, &path), [Ok(()), Ok(())], "{path}");
        }
        let (_cancel, _runtime, dotted) = team_worker(Some(&root.join(".")));
        assert_eq!(
            guard_file(&dotted, &display(&root.join("owned/a.rs"))),
            [Ok(()), Ok(())]
        );
    }

    // Given: a team Worker whose workspace root is a temporary directory
    // When: edit or write names a path outside the workspace, the workspace
    //       itself, a parent-directory escape, another worker's path, an
    //       unclaimed task's path, a path no task names, or no path at all
    // Then: the guard rejects each
    #[tokio::test]
    async fn edit_and_write_outside_owned_paths_are_rejected() {
        let workspace = tempfile::tempdir().unwrap();
        let root = workspace.path();
        let elsewhere = tempfile::tempdir().unwrap();
        let (_cancel, _runtime, state) = team_worker(Some(root));
        for path in [
            display(&elsewhere.path().join("owned/a.rs")),
            display(root),
            String::new(),
            "../owned/a.rs".into(),
            "owned/../theirs/a.rs".into(),
            display(&root.join("owned/../theirs/a.rs")),
            "theirs/a.rs".into(),
            "free/a.rs".into(),
            "unlisted.rs".into(),
        ] {
            for decision in guard_file(&state, &path) {
                assert!(decision.is_err(), "{path}");
            }
        }
        for name in ["edit", "write"] {
            assert!(state.guard_team_artifact(name, &json!({})).is_err());
        }
    }

    // Given: a team Worker whose executor has no workspace root
    // When: edit or write names an owned path, relative or absolute
    // Then: the guard rejects it, because it cannot tell where the tool writes
    #[tokio::test]
    async fn edit_and_write_without_a_workspace_root_are_rejected() {
        let workspace = tempfile::tempdir().unwrap();
        let (_cancel, _runtime, state) = team_worker(None);
        for path in [
            "owned/a.rs".to_string(),
            display(&workspace.path().join("owned/a.rs")),
        ] {
            for decision in guard_file(&state, &path) {
                assert!(decision.is_err(), "{path}");
            }
        }
    }

    // Given: a team Worker whose workspace root is a temporary directory, not
    //        the process working directory, holding an owned directory with a
    //        symlink to another worker's directory
    // When: edit or write names a path through the symlink, relative or absolute,
    //       or the symlink itself
    // Then: the guard rejects it, because it looks for symlinks below the
    //       workspace root where the tool writes; a real owned directory next to
    //       the symlink is still admitted
    #[cfg(unix)]
    #[tokio::test]
    async fn edit_and_write_through_symlinks_below_the_workspace_root_are_rejected() {
        let workspace = tempfile::tempdir().unwrap();
        let root = workspace.path();
        std::fs::create_dir_all(root.join("owned/real")).unwrap();
        std::fs::create_dir_all(root.join("theirs")).unwrap();
        std::os::unix::fs::symlink(root.join("theirs"), root.join("owned/link")).unwrap();
        let (_cancel, _runtime, state) = team_worker(Some(root));
        for path in [
            "owned/link/a.rs".to_string(),
            "./owned/link/a.rs".into(),
            display(&root.join("owned/link/a.rs")),
            "owned/link".into(),
        ] {
            for decision in guard_file(&state, &path) {
                assert!(decision.is_err(), "{path}");
            }
        }
        assert_eq!(guard_file(&state, "owned/real/a.rs"), [Ok(()), Ok(())]);
    }

    fn tool_results(state: &super::LoopState) -> Vec<(String, bool, String)> {
        state
            .context
            .messages
            .iter()
            .flat_map(|message| &message.content)
            .filter_map(|block| match block {
                ContentBlock::ToolResult {
                    tool_call_id,
                    content,
                    is_error,
                } => Some((
                    tool_call_id.clone(),
                    *is_error,
                    content
                        .iter()
                        .filter_map(|item| match item {
                            ToolResultContent::Text { text } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect(),
                )),
                _ => None,
            })
            .collect()
    }

    // Given: a team Worker run whose executor resolves patches in a workspace
    //        and whose task owns `owned`
    // When: one batch applies an owned patch, a patch that also touches an
    //       unowned path, and a patch that does not parse
    // Then: only the owned patch changes files; the mixed one is rejected by
    //       the team guard without touching its owned path; the unparseable one
    //       reaches the tool and fails in Codex's words
    #[tokio::test]
    async fn team_worker_batch_applies_only_owned_patches() {
        let workspace = tempfile::tempdir().unwrap();
        let root = workspace.path();
        let (_cancel, _runtime, mut state) = team_worker(Some(root));
        state
            .transition(event_bus::AgentRunPhase::Running, None)
            .unwrap();
        let model = Arc::clone(&state.shared.model);

        assert!(
            state
                .execute_tools(
                    vec![
                        (
                            "owned".into(),
                            "apply_patch".into(),
                            patch(&add("owned/a.rs"))
                        ),
                        (
                            "mixed".into(),
                            "apply_patch".into(),
                            patch(&format!("{}\n{}", add("owned/b.rs"), add("other/b.rs"))),
                        ),
                        (
                            "garbage".into(),
                            "apply_patch".into(),
                            json!({ "input": "garbage" }),
                        ),
                    ],
                    &crate::AgentInvocationContext::default(),
                    model.as_ref(),
                )
                .await
        );

        let results = tool_results(&state);
        assert_eq!(
            results
                .iter()
                .map(|(id, is_error, _)| (id.as_str(), *is_error))
                .collect::<Vec<_>>(),
            [("owned", false), ("mixed", true), ("garbage", true)],
            "{results:?}"
        );
        assert!(results[1].2.contains("nothing was changed"), "{results:?}");
        assert!(
            results[2].2.starts_with("apply_patch verification failed"),
            "{results:?}"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("owned/a.rs")).unwrap(),
            "x\n"
        );
        assert!(!root.join("owned/b.rs").exists());
        assert!(!root.join("other").exists());
    }

    /// Every regular file below `root` with its contents, keyed by its path
    /// relative to `root`. Symlinks are listed by neither name nor target.
    fn files(root: &Path) -> std::collections::BTreeMap<String, String> {
        let mut found = std::collections::BTreeMap::new();
        let mut pending = vec![root.to_path_buf()];
        while let Some(directory) = pending.pop() {
            for entry in std::fs::read_dir(&directory).unwrap() {
                let entry = entry.unwrap();
                let kind = entry.file_type().unwrap();
                if kind.is_dir() {
                    pending.push(entry.path());
                } else if kind.is_file() {
                    found.insert(
                        display(entry.path().strip_prefix(root).unwrap()),
                        std::fs::read_to_string(entry.path()).unwrap(),
                    );
                }
            }
        }
        found
    }

    // Given: a team Worker run whose workspace root is a temporary directory,
    //        not the process working directory, holding owned files and a
    //        symlink from its owned directory to another worker's directory
    // When: one batch writes and edits owned paths relative, `./`-prefixed and
    //       absolute, and writes and edits through the symlink
    // Then: exactly the owned files change below the workspace root; the team
    //       guard rejects the symlinked calls, so the other directory is untouched
    #[cfg(unix)]
    #[tokio::test]
    async fn team_worker_batch_changes_only_owned_files() {
        let workspace = tempfile::tempdir().unwrap();
        let root = workspace.path();
        std::fs::create_dir_all(root.join("owned")).unwrap();
        std::fs::create_dir_all(root.join("theirs")).unwrap();
        for file in ["owned/edited.rs", "owned/dotted.rs", "theirs/kept.rs"] {
            std::fs::write(root.join(file), "a\n").unwrap();
        }
        std::os::unix::fs::symlink(root.join("theirs"), root.join("owned/link")).unwrap();
        let (_cancel, _runtime, mut state) = team_worker(Some(root));
        state
            .transition(event_bus::AgentRunPhase::Running, None)
            .unwrap();
        let model = Arc::clone(&state.shared.model);
        let write = |path: String, content: &str| json!({ "path": path, "content": content });
        let edit = |path: String| json!({ "path": path, "old_string": "a", "new_string": "b" });

        assert!(
            state
                .execute_tools(
                    vec![
                        (
                            "relative".into(),
                            "write".into(),
                            write("owned/a.rs".into(), "a\n")
                        ),
                        (
                            "absolute".into(),
                            "write".into(),
                            write(display(&root.join("owned/b.rs")), "b\n"),
                        ),
                        (
                            "dotted".into(),
                            "write".into(),
                            write("./owned/c.rs".into(), "c\n")
                        ),
                        ("edit".into(), "edit".into(), edit("owned/edited.rs".into())),
                        (
                            "dotted-edit".into(),
                            "edit".into(),
                            edit("./owned/dotted.rs".into())
                        ),
                        (
                            "linked-write".into(),
                            "write".into(),
                            write("owned/link/c.rs".into(), "c\n"),
                        ),
                        (
                            "linked-edit".into(),
                            "edit".into(),
                            edit("owned/link/kept.rs".into())
                        ),
                    ],
                    &crate::AgentInvocationContext::default(),
                    model.as_ref(),
                )
                .await
        );

        let results = tool_results(&state);
        assert_eq!(
            results
                .iter()
                .map(|(id, is_error, _)| (id.as_str(), *is_error))
                .collect::<Vec<_>>(),
            [
                ("relative", false),
                ("absolute", false),
                ("dotted", false),
                ("edit", false),
                ("dotted-edit", false),
                ("linked-write", true),
                ("linked-edit", true),
            ],
            "{results:?}"
        );
        assert_eq!(
            files(root),
            [
                ("owned/a.rs", "a\n"),
                ("owned/b.rs", "b\n"),
                ("owned/c.rs", "c\n"),
                ("owned/dotted.rs", "b\n"),
                ("owned/edited.rs", "b\n"),
                ("theirs/kept.rs", "a\n"),
            ]
            .map(|(path, content)| (path.to_string(), content.to_string()))
            .into()
        );
    }

    // Given: a team Worker run whose workspace root is configured as a relative
    //        path, which every tool call scopes its arguments against twice
    // When: it writes an owned path
    // Then: the root was made absolute against the process directory, so the
    //       team guard admits exactly the path the write tool receives
    #[tokio::test]
    async fn relative_workspace_root_is_guarded_where_the_tool_writes() {
        // Nothing exists there, so the write fails without creating files.
        let relative = Path::new("evorch-team-relative-root-does-not-exist");
        let (_cancel, _runtime, mut state) = team_worker(Some(relative));
        let root = state.shared.executor.default_cwd().unwrap();
        assert_eq!(root, std::env::current_dir().unwrap().join(relative));
        let mut events = state.shared.bus.subscribe();
        state
            .transition(event_bus::AgentRunPhase::Running, None)
            .unwrap();
        let model = Arc::clone(&state.shared.model);

        assert!(
            state
                .execute_tools(
                    vec![(
                        "write".into(),
                        "write".into(),
                        json!({ "path": "owned/a.rs", "content": "a\n" }),
                    )],
                    &crate::AgentInvocationContext::default(),
                    model.as_ref(),
                )
                .await
        );

        let input = events
            .drain_pending_snapshot()
            .into_iter()
            .find_map(|event| match event.kind {
                event_bus::EventKind::Tool(event_bus::ToolEvent::ToolStarted {
                    call_id,
                    input,
                    ..
                }) if call_id == "write" => input,
                _ => None,
            })
            .expect("the guard admitted the write, so the tool started");
        assert_eq!(input["path"], json!(display(&root.join("owned/a.rs"))));
        assert!(!root.exists());
    }
}
