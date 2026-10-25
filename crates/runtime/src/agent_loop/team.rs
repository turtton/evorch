use super::LoopState;
use crate::ownership::Lease;
use crate::team::TeamBoard;
use apply_patch::TargetsError;
use providers::ToolSpec;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use tools::ToolResult;

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

    pub(super) fn guard_team_artifact(
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
            "edit" | "write" => {
                let path = input
                    .get("path")
                    .and_then(serde_json::Value::as_str)
                    .ok_or("write/edit path missing")?;
                team.board
                    .authorize_path(
                        &self.task.run_id.to_string(),
                        std::path::Path::new(path),
                        team.now(),
                    )
                    .map_err(|e| e.to_string())
            }
            // The tool resolves relative patch paths against the executor's
            // workspace, so ownership is judged against the same root.
            "apply_patch" => authorize_patch(
                &team.board,
                &self.task.run_id.to_string(),
                self.shared.executor.default_cwd().as_deref(),
                input,
                team.now(),
            ),
            _ => Ok(()),
        }
    }
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
    let Some(root) = root else {
        return Err(denied("the workspace root is unknown".into()));
    };
    // `targets` drops `.` components the same way, so a lexical prefix check
    // matches how the tool resolves each path.
    let root: PathBuf = root.components().collect();
    for target in targets {
        let Ok(relative) = target.resolved.strip_prefix(&root) else {
            return Err(denied(format!(
                "{} is outside the workspace",
                target.raw.display()
            )));
        };
        board
            .authorize_path_under(worker, &root, relative, now)
            .map_err(|error| denied(format!("{}: {error}", target.raw.display())))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;

    use providers::{ContentBlock, ToolResultContent};
    use serde_json::{Value, json};

    use super::authorize_patch;
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
        let (_cancel, cancel_rx) = tokio::sync::watch::channel(crate::RunInterrupt::None);
        let (_runtime, mut state) = super::super::delegate_cleanup_tests::fixture(cancel_rx);
        let workspace = tempfile::tempdir().unwrap();
        let root = workspace.path();
        let executor = tools::ToolExecutor::with_standard_tools(
            Arc::clone(&state.shared.bus),
            Arc::new(sandbox::DirectSandbox::new_unchecked()),
        )
        .with_policy(sandbox::ApprovalPolicy::allow_all());
        executor.set_default_cwd(root.to_path_buf());
        state.shared.executor = Arc::new(executor);
        state.task.role = agents::Role::Worker;
        state.policy = crate::ExecutionPolicy::for_role(agents::Role::Worker);
        state.task.config.topology = crate::CoordinationTopology::DynamicTeam { max_workers: 3 };
        let team = crate::team_context::TeamContext::new(state.task.run_id, 3);
        team.board
            .enqueue(TaskSpec {
                id: "mine".into(),
                paths: vec!["owned".into()],
            })
            .unwrap();
        team.board
            .claim("mine", &state.task.run_id.to_string(), team.now())
            .unwrap();
        state.task.config.team = Some(team);
        assert!(state.team_worker());
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
}
