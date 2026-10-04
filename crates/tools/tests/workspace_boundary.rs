use std::sync::Arc;

use event_bus::EventBus;
use sandbox::DirectSandbox;
use serde_json::json;
use tools::{Permissions, Tool, ToolError, ToolExecutionContext, ToolExecutor, ToolResult};

fn context() -> ToolExecutionContext {
    ToolExecutionContext {
        run_id: "candidate".into(),
        thread_id: None,
        call_id: None,
    }
}

#[tokio::test]
async fn recorded_workspace_allows_local_reads_and_writes_but_denies_future_evidence() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("workspace");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("input.txt"), "checkpoint input").unwrap();
    let future = dir.path().join("baseline.json");
    std::fs::write(&future, "future evaluation evidence").unwrap();
    let executor = Arc::new(ToolExecutor::with_standard_tools_in(
        Arc::new(EventBus::new(32)),
        Arc::new(DirectSandbox::new_unchecked()),
        Some(root.clone()),
    ));
    executor.set_workspace_boundary(root.clone()).unwrap();

    let read = executor
        .execute(&context(), "read", "local", json!({"path":"input.txt"}))
        .await
        .unwrap();
    assert!(read.content.contains("checkpoint input"));
    executor
        .execute(
            &context(),
            "write",
            "local-write",
            json!({"path":"output.txt", "content":"candidate output"}),
        )
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(root.join("output.txt")).unwrap(),
        "candidate output"
    );

    for args in [
        json!({"path":"../baseline.json"}),
        json!({"file_path":future}),
    ] {
        assert!(matches!(
            executor
                .execute(&context(), "read", "outside", args.clone())
                .await,
            Err(ToolError::ExecutionDenied { .. })
        ));
        assert!(matches!(
            executor.validate_call(context(), "read".into(), "prepared".into(), args),
            Err(ToolError::ExecutionDenied { .. })
        ));
    }
    assert!(matches!(
        executor
            .execute(
                &context(),
                "grep",
                "outside-grep",
                json!({"path":dir.path(),"pattern":"future"})
            )
            .await,
        Err(ToolError::ExecutionDenied { .. })
    ));
    assert!(matches!(
        executor
            .execute(
                &context(),
                "write",
                "outside-write",
                json!({"path":"../new.txt","content":"tamper"})
            )
            .await,
        Err(ToolError::ExecutionDenied { .. })
    ));
    assert!(!dir.path().join("new.txt").exists());

    // A trial-created FIFO must be rejected before a host-side blocking read.
    let pipe = root.join("blocking-pipe");
    rustix::fs::mknodat(
        rustix::fs::CWD,
        &pipe,
        rustix::fs::FileType::Fifo,
        rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
        0,
    )
    .unwrap();
    assert!(matches!(
        executor.validate_call(
            context(),
            "read".into(),
            "pipe".into(),
            json!({"path":pipe})
        ),
        Err(ToolError::ExecutionDenied { .. })
    ));
}

#[cfg(unix)]
#[tokio::test]
async fn symlinks_cannot_expose_evaluation_artifacts_and_prepared_calls_are_rechecked() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("workspace");
    std::fs::create_dir(&root).unwrap();
    let input = root.join("input.txt");
    let future = dir.path().join("baseline.json");
    std::fs::write(&input, "checkpoint").unwrap();
    std::fs::write(&future, "future").unwrap();
    let executor = Arc::new(ToolExecutor::with_standard_tools_in(
        Arc::new(EventBus::new(32)),
        Arc::new(DirectSandbox::new_unchecked()),
        Some(root.clone()),
    ));
    executor.set_workspace_boundary(root.clone()).unwrap();
    let prepared = executor
        .validate_call(
            context(),
            "read".into(),
            "prepared".into(),
            json!({"path":"input.txt"}),
        )
        .unwrap()
        .scope_approved();
    std::fs::remove_file(&input).unwrap();
    std::os::unix::fs::symlink(&future, &input).unwrap();
    assert!(matches!(
        prepared.execute().await,
        Err(ToolError::ExecutionDenied { .. })
    ));
    std::os::unix::fs::symlink(dir.path(), root.join("outside-dir")).unwrap();
    assert!(matches!(
        executor
            .execute(
                &context(),
                "write",
                "linked-write",
                json!({"path":"outside-dir/new.txt","content":"tamper"})
            )
            .await,
        Err(ToolError::ExecutionDenied { .. })
    ));
    assert!(!dir.path().join("new.txt").exists());
}

#[tokio::test]
async fn ordinary_executor_retains_its_existing_path_policy() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("input.txt");
    std::fs::write(&path, "ordinary input").unwrap();
    let executor = ToolExecutor::with_standard_tools(
        Arc::new(EventBus::new(8)),
        Arc::new(DirectSandbox::new_unchecked()),
    );
    let read = executor
        .execute(&context(), "read", "ordinary", json!({"path":path}))
        .await
        .unwrap();
    assert!(read.content.contains("ordinary input"));
}

#[tokio::test]
async fn fresh_large_outputs_publish_trial_local_artifacts_that_remain_readable() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("workspace");
    std::fs::create_dir(&root).unwrap();
    let mut executor = ToolExecutor::with_standard_tools_in(
        Arc::new(EventBus::new(16)),
        Arc::new(DirectSandbox::new_unchecked()),
        Some(root.clone()),
    );
    executor.register(Arc::new(LargeOutput)).unwrap();
    executor.set_workspace_boundary(root.clone()).unwrap();
    let result = executor
        .execute(&context(), "fixture-output", "large-output", json!({}))
        .await
        .unwrap();
    let path = result.detail.as_ref().unwrap()["output_artifact"]["path"]
        .as_str()
        .unwrap();
    assert!(std::path::Path::new(path).starts_with(root.join(".benchmark-tool-output")));
    assert!(result.content.contains(path));
    let read = executor
        .execute(
            &context(),
            "read",
            "artifact",
            json!({"path":path,"offset":1,"limit":1}),
        )
        .await
        .unwrap();
    assert!(read.content.contains('1'));
    assert_eq!(std::fs::read_to_string(path).unwrap().lines().count(), 400);
}

struct LargeOutput;

#[async_trait::async_trait]
impl Tool for LargeOutput {
    fn name(&self) -> &'static str {
        "fixture-output"
    }

    fn schema(&self) -> serde_json::Value {
        json!({"type":"object", "properties":{}, "additionalProperties":false})
    }

    fn permissions(&self) -> Permissions {
        Permissions {
            fs_read: false,
            fs_write: false,
            process_spawn: false,
            network: false,
        }
    }

    async fn execute(&self, _: serde_json::Value) -> Result<ToolResult, ToolError> {
        Ok(ToolResult::success("1\n".repeat(400)))
    }
}
