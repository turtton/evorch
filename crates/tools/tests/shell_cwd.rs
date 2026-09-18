use std::sync::Arc;

use sandbox::DirectSandbox;
use serde_json::json;
use tools::{Shell, Tool};

#[tokio::test]
async fn default_cwd_is_used_when_argument_is_absent() {
    // Given: a configured project directory, distinct from the process cwd.
    let directory = tempfile::tempdir().expect("project directory");
    let shell = Shell::new(Arc::new(DirectSandbox::new_unchecked()))
        .with_default_cwd(directory.path().to_path_buf());
    // When: pwd runs without a cwd argument.
    let result = shell.execute(json!({"command": "pwd"})).await.expect("pwd");
    // Then: the child runs in the project.
    assert_eq!(
        result.content,
        format!("exit_code: 0\n{}\n", directory.path().display())
    );
}

#[tokio::test]
async fn argument_cwd_overrides_default() {
    // Given: different default and explicit directories.
    let default = tempfile::tempdir().expect("default");
    let explicit = tempfile::tempdir().expect("explicit");
    let shell = Shell::new(Arc::new(DirectSandbox::new_unchecked()))
        .with_default_cwd(default.path().to_path_buf());
    // When: pwd has an explicit cwd.
    let result = shell
        .execute(json!({"command": "pwd", "cwd": explicit.path()}))
        .await
        .expect("pwd");
    // Then: the argument wins.
    assert_eq!(
        result.content,
        format!("exit_code: 0\n{}\n", explicit.path().display())
    );
}

#[tokio::test]
async fn process_cwd_is_preserved_without_default() {
    // Given: the legacy constructor.
    let shell = Shell::new(Arc::new(DirectSandbox::new_unchecked()));
    let cwd = std::env::current_dir().expect("process cwd");
    // When: no directory is supplied.
    let result = shell.execute(json!({"command": "pwd"})).await.expect("pwd");
    // Then: the process directory is inherited.
    assert_eq!(result.content, format!("exit_code: 0\n{}\n", cwd.display()));
}

#[tokio::test]
async fn pty_uses_default_cwd_when_argument_is_absent() {
    // Given: a project directory for an actual PTY execution.
    let directory = tempfile::tempdir().expect("project directory");
    let shell = Shell::new(Arc::new(DirectSandbox::new_unchecked()))
        .with_default_cwd(directory.path().to_path_buf());
    // When: interactive pwd runs without a cwd argument.
    let result = shell
        .execute(json!({"command": "pwd", "interactive": true, "timeout_ms": 2000}))
        .await
        .expect("PTY pwd");
    // Then: the PTY does not fall back to the process cwd.
    let normalized = result.content.replace("\r\n", "\n");
    assert_eq!(
        normalized
            .strip_prefix("exit_code: 0\n")
            .expect("successful exit")
            .trim(),
        directory.path().to_str().expect("UTF-8 path")
    );
}

#[tokio::test]
async fn standard_executor_injects_project_cwd() {
    // Given: an executor constructed for a project directory.
    let directory = tempfile::tempdir().expect("project");
    let executor = tools::ToolExecutor::with_standard_tools_in(
        Arc::new(event_bus::EventBus::new(16)),
        Arc::new(DirectSandbox::new_unchecked()),
        Some(directory.path().to_path_buf()),
    );
    let context = tools::ToolExecutionContext {
        run_id: "cwd-test".into(),
        thread_id: None,
        call_id: None,
    };
    // When: the registered shell is invoked without a cwd argument.
    let result = executor
        .execute(&context, "shell", "pwd-call", json!({"command": "pwd"}))
        .await
        .expect("pwd");
    // Then: construction context reaches the actual child process.
    assert_eq!(
        result.content,
        format!("exit_code: 0\n{}\n", directory.path().display())
    );
}
