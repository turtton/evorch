//! Tools that start a process register it in the core's spawn ledger, and start
//! none once its fence is closed. The children are real processes.

use std::sync::Arc;

use sandbox::{
    ChildKind, CommandSpec, Sandbox, SandboxError, SpawnLedger, WrappedCommand,
    ledger::ChildActivity,
};
use tools::{GitDiff, Grep, Tool, ToolError};

/// Wraps every command into a shell that stops itself, so the child stays
/// alive until the ledger kills it.
struct StopsItself;

impl Sandbox for StopsItself {
    fn wrap(&self, _: CommandSpec) -> Result<WrappedCommand, SandboxError> {
        Ok(WrappedCommand {
            program: "sh".into(),
            args: vec!["-c".into(), "kill -STOP $$".into()],
            cwd: None,
            inherit_env: true,
            env: Vec::new(),
        })
    }
}

#[tokio::test]
async fn a_git_diff_child_is_in_the_ledger_until_the_ledger_stops_and_reaps_it() {
    // Given: git_diff whose process stops itself, registered in a ledger.
    let ledger = SpawnLedger::new();
    let tool = GitDiff::new(Arc::new(StopsItself)).with_spawn_ledger(ledger.clone());
    let mut activity = ledger.watch_children();
    let call = tokio::spawn(async move { tool.execute(serde_json::json!({})).await });

    // When: the child is registered, and the core stops every child.
    activity
        .wait_for(|activity: &ChildActivity| activity.live == 1)
        .await
        .unwrap();
    let children = ledger.children();
    assert_eq!(children.len(), 1);
    assert_eq!(children[0].kind, ChildKind::Other);
    assert_eq!(children[0].label.as_deref(), Some("git-diff"));
    ledger.close_fence();
    assert_eq!(ledger.stop_all_children(), 1);

    // Then: the call ends and the leader's reap is confirmed.
    let result = call.await.unwrap();
    assert!(
        matches!(result, Err(ToolError::Io { .. })),
        "a killed git is a failed call: {result:?}"
    );
    ledger.reap_all_children().await.unwrap();
    let stats = ledger.child_stats();
    assert_eq!(
        (stats.registered, stats.reaped, stats.unconfirmed),
        (1, 1, 0)
    );
}

#[tokio::test]
async fn a_closed_fence_starts_no_git_and_no_rg() {
    // Given: a ledger whose spawn fence is closed.
    let ledger = SpawnLedger::new();
    ledger.close_fence();
    let directory = tempfile::tempdir().unwrap();

    // When: the tools try to start their processes.
    let diff = GitDiff::new(Arc::new(StopsItself))
        .with_spawn_ledger(ledger.clone())
        .execute(serde_json::json!({}))
        .await;
    let grep = Grep::default()
        .with_spawn_ledger(ledger.clone())
        .execute(serde_json::json!({"pattern": "x", "path": directory.path()}))
        .await;

    // Then: both fail to spawn and nothing was registered.
    assert!(
        matches!(diff, Err(ToolError::SpawnFailed { .. })),
        "{diff:?}"
    );
    assert!(
        matches!(grep, Err(ToolError::SpawnFailed { .. })),
        "{grep:?}"
    );
    assert_eq!(ledger.child_stats().registered, 0);
}

#[tokio::test]
async fn a_finished_rg_is_reaped_by_its_owner() {
    // Given: grep registered in a ledger and a directory to search.
    let ledger = SpawnLedger::new();
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("a.txt"), "needle\n").unwrap();
    let grep = Grep::default().with_spawn_ledger(ledger.clone());

    // When: the search runs to its end.
    let result = grep
        .execute(serde_json::json!({"pattern": "needle", "path": directory.path()}))
        .await
        .unwrap();

    // Then: it found the line and the ledger saw the leader reaped.
    assert!(result.content.contains("needle"), "{}", result.content);
    ledger.reap_all_children().await.unwrap();
    let stats = ledger.child_stats();
    assert_eq!(
        (stats.registered, stats.reaped, stats.unconfirmed),
        (1, 1, 0)
    );
}

#[tokio::test]
async fn an_executor_ledger_reaches_the_standard_tools_and_its_forks() {
    // Given: an executor with its standard tools, then given a closed ledger,
    // and a fork of it.
    let ledger = SpawnLedger::new();
    let directory = tempfile::tempdir().unwrap();
    let executor = tools::ToolExecutor::with_standard_tools_in(
        Arc::new(event_bus::EventBus::new(32)),
        Arc::new(StopsItself),
        Some(directory.path().to_path_buf()),
    )
    .with_spawn_ledger(ledger.clone());
    let fork = executor.fork_with_run_tools([]).unwrap();
    ledger.close_fence();
    let ctx = tools::ToolExecutionContext {
        run_id: "run-test".into(),
        ..Default::default()
    };

    // When: git_diff, grep and shell try to start a process.
    for executor in [&executor, &fork] {
        let calls = [
            ("git_diff", serde_json::json!({})),
            (
                "grep",
                serde_json::json!({"pattern": "x", "path": directory.path()}),
            ),
            ("shell", serde_json::json!({"command": "true"})),
        ];
        for (name, args) in calls {
            let result = executor.execute(&ctx, name, "call", args).await;

            // Then: none starts, and none was registered.
            assert!(
                result.is_err() || result.as_ref().is_ok_and(|result| result.is_error),
                "{name} started under a closed fence: {result:?}"
            );
        }
    }
    assert_eq!(ledger.child_stats().registered, 0);
}

fn checker(ledger: &SpawnLedger, directory: &std::path::Path) -> tools::post_edit::CommentChecker {
    use std::os::unix::fs::PermissionsExt;
    use tools::post_edit::PostEditHook;
    let binary = directory.join("checker");
    std::fs::write(&binary, "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
    let config = config::CommentCheckerConfig {
        binary: binary.to_str().unwrap().into(),
        ..Default::default()
    };
    let checker = tools::post_edit::CommentChecker::resolve(
        Arc::new(sandbox::DirectSandbox::new_unchecked()),
        &config,
    )
    .expect("the checker is executable");
    checker.set_spawn_ledger(ledger.clone());
    checker
}

fn input(directory: &std::path::Path) -> tools::post_edit::PostEditInput {
    tools::post_edit::PostEditInput {
        tool_name: "write",
        file_path: directory.join("a.rs"),
        content: Some("fn main() {}\n".into()),
        old_string: None,
        new_string: None,
        edits: None,
    }
}

#[tokio::test]
async fn a_comment_checker_and_its_supervisor_are_owned_by_the_ledger() {
    use tools::post_edit::{PostEditHook, PostEditOutcome};

    // Given: a checker registered in a ledger.
    let ledger = SpawnLedger::new();
    let directory = tempfile::tempdir().unwrap();
    let hook = checker(&ledger, directory.path());

    // When: it checks a file.
    let outcome = hook.check(&input(directory.path())).await;

    // Then: the checker passed, its supervisor is a joinable worker, and its
    // child was reaped by its owner.
    assert_eq!(outcome, PostEditOutcome::Pass);
    ledger.join_workers().await.unwrap();
    ledger.reap_all_children().await.unwrap();
    let stats = ledger.child_stats();
    assert_eq!(
        (stats.registered, stats.reaped, stats.unconfirmed),
        (1, 1, 0)
    );
    assert_eq!(ledger.worker_stats().spawned, 1);
}

#[tokio::test]
async fn a_closed_fence_starts_no_comment_checker() {
    use tools::post_edit::{PostEditHook, PostEditOutcome, UnavailableReason};

    // Given: a checker whose ledger is fenced.
    let ledger = SpawnLedger::new();
    ledger.close_fence();
    let directory = tempfile::tempdir().unwrap();

    // When: it checks a file.
    let outcome = checker(&ledger, directory.path())
        .check(&input(directory.path()))
        .await;

    // Then: it is unavailable, nothing started, and no supervisor is left.
    assert!(
        matches!(
            outcome,
            PostEditOutcome::Unavailable {
                reason: UnavailableReason::SpawnFailed,
                ..
            }
        ),
        "{outcome:?}"
    );
    assert_eq!(ledger.child_stats().registered, 0);
    assert_eq!(ledger.worker_stats().spawned, 0);
}

fn lsp() -> tools::lsp::LspDiagnostics {
    tools::lsp::LspDiagnostics::new(
        Arc::new(StopsItself),
        CommandSpec {
            program: "unused".into(),
            args: Vec::new(),
            cwd: None,
            extra_env: Vec::new(),
        },
        "rust".into(),
    )
}

#[tokio::test]
async fn an_lsp_server_given_through_the_tool_trait_is_owned_by_the_ledger() {
    // Given: an lsp tool handed the ledger the way an executor hands it.
    let ledger = SpawnLedger::new();
    let tool = Arc::new(lsp());
    tool.set_spawn_ledger(ledger.clone());
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("a.rs");
    std::fs::write(&file, "fn main() {}").unwrap();
    let mut activity = ledger.watch_children();
    let call = {
        let tool = tool.clone();
        tokio::spawn(async move { tool.execute(serde_json::json!({"path": file})).await })
    };

    // When: the server is registered, and the core stops every child.
    activity
        .wait_for(|activity: &ChildActivity| activity.live == 1)
        .await
        .unwrap();
    ledger.close_fence();
    assert_eq!(ledger.stop_all_children(), 1);

    // Then: the call ends and the server's reap is confirmed.
    let result = call.await.unwrap();
    assert!(result.is_err() || result.is_ok_and(|result| result.is_error));
    ledger.reap_all_children().await.unwrap();
    let stats = ledger.child_stats();
    assert_eq!(
        (stats.registered, stats.reaped, stats.unconfirmed),
        (1, 1, 0)
    );
}

#[tokio::test]
async fn a_closed_fence_starts_no_lsp_server() {
    // Given: an lsp tool whose ledger is fenced.
    let ledger = SpawnLedger::new();
    ledger.close_fence();
    let tool = lsp();
    tool.set_spawn_ledger(ledger.clone());
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("a.rs");
    std::fs::write(&file, "fn main() {}").unwrap();

    // When: it is asked for diagnostics.
    let result = tool.execute(serde_json::json!({"path": file})).await;

    // Then: no server started.
    assert!(result.is_err() || result.is_ok_and(|result| result.is_error));
    assert_eq!(ledger.child_stats().registered, 0);
}
