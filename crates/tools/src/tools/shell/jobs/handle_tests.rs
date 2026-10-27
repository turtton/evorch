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
        ..Default::default()
    }
}

async fn start(shell: &Shell, ctx: &ToolExecutionContext, command: &str) -> String {
    let result = shell
        .execute_with_context(ctx, json!({"command":command, "yield_ms":0}))
        .await
        .unwrap();
    let state = &result.detail.as_ref().unwrap()["shell_job"];
    let uid = state["job_uid"].as_str().unwrap();
    assert!(uuid::Uuid::parse_str(uid).is_ok());
    assert!(!result.content.contains(uid));
    let handle = state["job_id"].as_str().unwrap().to_owned();
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

fn number(handle: &str) -> u64 {
    handle.strip_prefix("job-").unwrap().parse().unwrap()
}

#[tokio::test]
async fn rebuilt_shell_for_the_same_run_rejects_every_stale_control() {
    let ctx = owner();
    let old = Shell::new(Arc::new(DirectSandbox::new_unchecked()));
    let stale = start(&old, &ctx, "true").await;
    observe(&old, &ctx, &stale).await;
    let rebuilt = Shell::new(Arc::new(DirectSandbox::new_unchecked()));
    let fresh = start(&rebuilt, &ctx, "read value").await;
    assert!(number(&fresh) > number(&stale));
    drop(old);
    assert_unavailable(&rebuilt, &ctx, &stale).await;
    let job = rebuilt.jobs.by_handle.lock().unwrap()[&fresh].clone();
    assert!(job.running());
    assert!(!*job.cancel.borrow());
    control(&rebuilt, &ctx, &fresh, "stop").await.unwrap();
    observe(&rebuilt, &ctx, &fresh).await;
}

#[tokio::test]
async fn released_handles_never_control_a_successor_or_reset_the_counter() {
    let shell = Shell::new(Arc::new(DirectSandbox::new_unchecked()));
    let ctx = owner();
    let mut handles: Vec<String> = Vec::new();
    for _ in 0..3 {
        let handle = start(&shell, &ctx, "true").await;
        if let Some(previous) = handles.last() {
            assert!(number(&handle) > number(previous));
        }
        observe(&shell, &ctx, &handle).await;
        handles.push(handle);
    }
    shell.release_shell_jobs(&ctx.run_id).unwrap();
    assert!(shell.jobs.jobs.lock().unwrap().is_empty());
    assert!(shell.jobs.by_handle.lock().unwrap().is_empty());
    let successor = start(&shell, &ctx, "read value").await;
    assert!(number(&successor) > number(handles.last().unwrap()));
    for handle in &handles {
        assert_unavailable(&shell, &ctx, handle).await;
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
    let mut handles: Vec<String> = Vec::new();
    for _ in 0..MAX_RETAINED {
        let handle = start(&shell, &ctx, "true").await;
        observe(&shell, &ctx, &handle).await;
        handles.push(handle);
    }
    let successor = start(&shell, &ctx, "read value").await;
    assert!(number(&successor) > number(handles.last().unwrap()));
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
    let mut handles: Vec<String> = Vec::new();
    while let Some(result) = tasks.join_next().await {
        handles.push(result.unwrap());
    }
    handles.sort_by_key(|handle| number(handle));
    assert_eq!(handles.len(), MAX_RUNNING);
    assert!(
        handles
            .windows(2)
            .all(|pair| number(&pair[0]) < number(&pair[1]))
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
async fn allocator_failure_never_launches_the_command() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("broken-state");
    std::fs::create_dir(&state).unwrap();
    let marker = dir.path().join("must-not-exist");
    let mut shell = Shell::new(Arc::new(DirectSandbox::new_unchecked()));
    Arc::get_mut(&mut shell.jobs).unwrap().allocator =
        crate::shell_handles::HandleAllocator::for_test(state);
    assert!(
        shell
            .execute_with_context(
                &owner(),
                json!({"command": format!("touch {}", marker.display()), "yield_ms": 0})
            )
            .await
            .is_err()
    );
    assert!(!marker.exists());
    assert!(shell.jobs.jobs.lock().unwrap().is_empty());
    assert!(shell.jobs.by_handle.lock().unwrap().is_empty());
}

#[tokio::test]
async fn full_unobserved_retention_refuses_sync_spawn_and_keeps_every_outcome() {
    let shell = Shell::new(Arc::new(DirectSandbox::new_unchecked()));
    let ctx = owner();
    for _ in 0..MAX_RETAINED {
        let handle = start(&shell, &ctx, "true").await;
        shell.wait_for_job(&ctx, &handle).await.unwrap();
    }
    let directory = tempfile::tempdir().unwrap();
    let result = tokio::time::timeout(
        Duration::from_secs(1),
        shell.execute_with_context(
            &ctx,
            json!({"command":"touch must-not-start", "cwd":directory.path()}),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(result.is_error);
    assert!(result.content.contains("32/32"));
    assert!(result.content.contains("finished, unobserved"));
    assert_eq!(result.content.matches("Poll your ").count(), 3);
    assert!(!directory.path().join("must-not-start").exists());
    assert_eq!(shell.jobs.jobs.lock().unwrap().len(), MAX_RETAINED);
    assert_eq!(shell.jobs.by_handle.lock().unwrap().len(), MAX_RETAINED);
    assert!(shell.has_unobserved_shell_jobs(&ctx.run_id));
}
