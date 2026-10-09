use super::*;
use crate::Tool;
use crate::tools::Shell;
use sandbox::DirectSandbox;
use serde_json::json;

fn owner() -> ToolExecutionContext {
    ToolExecutionContext {
        run_id: "owner".into(),
        thread_id: Some("thread".into()),
        call_id: None,
    }
}

async fn start(shell: &Shell, ctx: &ToolExecutionContext, command: &str) -> String {
    let result = shell
        .execute_with_context(ctx, json!({"command":command, "yield_ms":0}))
        .await
        .unwrap();
    let handle = result.detail.unwrap()["shell_job"]["job_id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        result
            .content
            .starts_with(&format!("shell job: {handle}\n"))
    );
    handle
}

async fn control(
    shell: &Shell,
    ctx: &ToolExecutionContext,
    handle: &str,
    action: &str,
) -> Result<ToolResult, ToolError> {
    shell
        .execute_with_context(ctx, json!({"action":action, "job_id":handle}))
        .await
}

async fn observe(shell: &Shell, ctx: &ToolExecutionContext, handle: &str) {
    shell.wait_for_job(ctx, handle).await.unwrap();
    control(shell, ctx, handle, "poll").await.unwrap();
}

async fn assert_unavailable(shell: &Shell, ctx: &ToolExecutionContext, handle: &str) {
    for action in ["poll", "stdin", "stop"] {
        assert!(matches!(control(shell, ctx, handle, action).await,
            Err(ToolError::InvalidArgs { detail }) if detail.contains("unavailable")));
    }
    assert!(matches!(
        shell.wait_for_job(ctx, handle).await,
        Err(ToolError::InvalidArgs { .. })
    ));
}

#[tokio::test]
async fn released_handles_never_control_a_successor_or_reset_the_counter() {
    let shell = Shell::new(Arc::new(DirectSandbox::new_unchecked()));
    let ctx = owner();
    for number in 0..3 {
        let handle = start(&shell, &ctx, "true").await;
        assert_eq!(handle, format!("job-{number}"));
        observe(&shell, &ctx, &handle).await;
    }
    shell.release_shell_jobs(&ctx.run_id).unwrap();
    assert!(shell.jobs.jobs.lock().unwrap().is_empty());
    assert!(shell.jobs.by_handle.lock().unwrap().is_empty());
    let successor = start(&shell, &ctx, "read value").await;
    assert_eq!(successor, "job-3");
    for number in 0..3 {
        assert_unavailable(&shell, &ctx, &format!("job-{number}")).await;
    }
    assert!(shell.has_running_shell_jobs(&ctx.run_id));
    let job = shell.jobs.by_handle.lock().unwrap()[&successor].clone();
    assert!(!*job.cancel.borrow());
    control(&shell, &ctx, &successor, "stop").await.unwrap();
    observe(&shell, &ctx, &successor).await;
}

#[tokio::test]
async fn retained_capacity_eviction_removes_both_indexes() {
    let shell = Shell::new(Arc::new(DirectSandbox::new_unchecked()));
    let ctx = owner();
    let mut handles = Vec::new();
    for _ in 0..MAX_RETAINED {
        let handle = start(&shell, &ctx, "true").await;
        observe(&shell, &ctx, &handle).await;
        handles.push(handle);
    }
    let successor = start(&shell, &ctx, "read value").await;
    assert_eq!(successor, format!("job-{MAX_RETAINED}"));
    let evicted: Vec<_> = {
        let jobs = shell.jobs.jobs.lock().unwrap();
        let by_handle = shell.jobs.by_handle.lock().unwrap();
        assert_eq!(jobs.len(), MAX_RETAINED);
        assert_eq!(by_handle.len(), MAX_RETAINED);
        assert!(
            jobs.values()
                .all(|job| Arc::ptr_eq(job, &by_handle[&job.handle]))
        );
        handles
            .into_iter()
            .filter(|handle| !by_handle.contains_key(handle))
            .collect()
    };
    assert_eq!(evicted.len(), 1);
    assert_unavailable(&shell, &ctx, &evicted[0]).await;
    assert!(shell.has_running_shell_jobs(&ctx.run_id));
    control(&shell, &ctx, &successor, "stop").await.unwrap();
    observe(&shell, &ctx, &successor).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_starts_allocate_unique_monotonic_handles() {
    let shell = Arc::new(Shell::new(Arc::new(DirectSandbox::new_unchecked())));
    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..MAX_RUNNING {
        let shell = shell.clone();
        tasks.spawn(async move { start(&shell, &owner(), "read value").await });
    }
    let mut handles = Vec::new();
    while let Some(result) = tasks.join_next().await {
        handles.push(result.unwrap());
    }
    handles.sort();
    assert_eq!(
        handles,
        (0..MAX_RUNNING)
            .map(|n| format!("job-{n}"))
            .collect::<Vec<_>>()
    );
    shell.drain_shell_jobs(&owner().run_id).await.unwrap();
}

#[tokio::test]
async fn handles_keep_run_and_thread_scope_and_internal_ids_are_rejected() {
    let shell = Shell::new(Arc::new(DirectSandbox::new_unchecked()));
    let ctx = owner();
    let handle = start(&shell, &ctx, "read value").await;
    let job = shell.jobs.by_handle.lock().unwrap()[&handle].clone();
    assert!(uuid::Uuid::parse_str(&job.id).is_ok());
    assert_unavailable(&shell, &ctx, &job.id).await;
    for foreign in [
        ToolExecutionContext {
            run_id: "other".into(),
            ..owner()
        },
        ToolExecutionContext {
            thread_id: Some("other".into()),
            ..owner()
        },
    ] {
        assert_unavailable(&shell, &foreign, &handle).await;
    }
    assert!(job.running());
    assert!(!*job.cancel.borrow());
    control(&shell, &ctx, &handle, "stop").await.unwrap();
    observe(&shell, &ctx, &handle).await;
}

#[tokio::test]
async fn running_summary_chooses_numeric_minimum_and_excludes_other_runs() {
    let shell = Shell::new(Arc::new(DirectSandbox::new_unchecked()));
    let ctx = owner();
    for number in 0..=10 {
        let handle = start(
            &shell,
            &ctx,
            if [2, 10].contains(&number) {
                "read value"
            } else {
                "true"
            },
        )
        .await;
        if ![2, 10].contains(&number) {
            observe(&shell, &ctx, &handle).await;
        }
    }
    let other = ToolExecutionContext {
        run_id: "other".into(),
        ..owner()
    };
    let foreign = start(&shell, &other, "read foreign").await;
    assert_eq!(
        shell.running_shell_job_summary(&ctx.run_id),
        Some(ShellJobSummary {
            handle: "job-2".into(),
            command_summary: "read value".into(),
        })
    );
    control(&shell, &ctx, "job-2", "stop").await.unwrap();
    observe(&shell, &ctx, "job-2").await;
    assert_eq!(
        shell.running_shell_job_summary(&ctx.run_id).unwrap().handle,
        "job-10"
    );
    control(&shell, &ctx, "job-10", "stop").await.unwrap();
    observe(&shell, &ctx, "job-10").await;
    assert_eq!(shell.running_shell_job_summary(&ctx.run_id), None);
    assert_eq!(shell.running_shell_job_summary("missing"), None);
    assert_eq!(
        shell
            .running_shell_job_summary(&other.run_id)
            .unwrap()
            .handle,
        foreign
    );
    shell.drain_shell_jobs(&other.run_id).await.unwrap();
}

#[test]
fn command_summary_flattens_newlines_and_truncates_at_character_boundaries() {
    assert_eq!(
        command_summary("echo one\necho two\rthree"),
        "echo one echo two three"
    );
    for text in ["a", "日", "🙂"] {
        assert_eq!(command_summary(&text.repeat(80)), text.repeat(80));
        for length in [81, 100] {
            let summary = command_summary(&text.repeat(length));
            assert_eq!(summary, format!("{}…", text.repeat(79)));
            assert_eq!(summary.chars().count(), 80);
        }
    }
    assert_eq!(command_summary(""), "");
}

#[tokio::test]
async fn summary_uses_the_combined_command_and_args() {
    let shell = Shell::new(Arc::new(DirectSandbox::new_unchecked()));
    let ctx = owner();
    shell
        .execute_with_context(
            &ctx,
            json!({"command":"read", "args":["value", "\n"], "yield_ms":0}),
        )
        .await
        .unwrap();
    assert_eq!(
        shell
            .running_shell_job_summary(&ctx.run_id)
            .unwrap()
            .command_summary,
        "read value  "
    );
    shell.drain_shell_jobs(&ctx.run_id).await.unwrap();
}
