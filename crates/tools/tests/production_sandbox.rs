//! production 用 composition root（`with_production_sandbox`）経由で構築した
//! 実行器の e2e テスト。
//!
//! bwrap が実際に隔離を提供することを前提とするため、テストは
//! `#[ignore = "bwrap 実行環境が必要"]` で実環境でのみ実行される。

use std::sync::Arc;

use event_bus::EventBus;
use sandbox::{BwrapConfig, BwrapSandbox, CommandSpec, Sandbox, SandboxError, WrappedCommand};
use tools::{ToolExecutionContext, ToolExecutor};

fn workspace_dir() -> tempfile::TempDir {
    // bwrap は /tmp を tmpfs として扱うため、一時領域はサンドボックスから
    // 見える作業ツリー内に作る。plain な tempdir() では隔離内から見えない。
    tempfile::Builder::new()
        .prefix("tools-production-")
        .tempdir_in(std::env::current_dir().expect("作業ディレクトリを取得できるはずです"))
        .expect("作業ツリー内に一時領域を作成できるはずです")
}

// Given: production 用 composition root で構築した実行器と一時ワークスペース / When: 承認なしで shell の pwd を実行 / Then: bwrap 内で正常終了し cwd が作業パスになる
#[tokio::test]
#[ignore = "bwrap 実行環境が必要"]
async fn production_executor_runs_shell_inside_bwrap() {
    let workspace = workspace_dir();
    let bus = Arc::new(EventBus::new(16));
    let executor = ToolExecutor::with_production_sandbox(
        Arc::clone(&bus),
        BwrapConfig::new(workspace.path().to_path_buf()),
    )
    .expect("bwrap 実行環境が必要です");

    let result = executor
        .execute(
            &ToolExecutionContext {
                run_id: "run-1".to_string(),
            },
            "shell",
            "call-production-pwd",
            serde_json::json!({ "command": "pwd && echo hello | tr a-z A-Z > output.rs && cat *.rs && echo err >&2" }),
        )
        .await
        .expect("承認なしの Shell 実行は成功するはずです");

    let expected = workspace.path().display().to_string();
    assert!(!result.is_error);
    assert_eq!(
        result.content,
        format!("exit_code: 0\n{expected}\nHELLO\nerr\n")
    );
    assert!(
        result.content.contains(&expected),
        "pwd の出力に作業パス {expected} が含まれない: {}",
        result.content
    );
}

struct ShellBoundarySandbox {
    inner: BwrapSandbox,
    command: &'static str,
}

impl Sandbox for ShellBoundarySandbox {
    fn wrap(&self, spec: CommandSpec) -> Result<WrappedCommand, SandboxError> {
        assert_eq!(spec.program, "sh", "Shell must select the interpreter");
        assert_eq!(spec.args, ["-c", self.command]);
        let wrapped = self.inner.wrap(spec)?;
        assert!(wrapped.args.ends_with(&[
            "sh".to_string(),
            "-c".to_string(),
            self.command.to_string(),
        ]));
        Ok(wrapped)
    }
}

async fn assert_shell_listing(command: &'static str) {
    // Given: standard tool registration and a real bwrap workspace with a hidden file.
    let workspace = workspace_dir();
    std::fs::write(workspace.path().join(".shell-listing-marker"), "marker")
        .expect("create listing fixture");
    let sandbox = ShellBoundarySandbox {
        inner: BwrapSandbox::detect(BwrapConfig::new(workspace.path().to_path_buf()))
            .expect("bwrap execution environment required"),
        command,
    };
    let executor =
        ToolExecutor::with_standard_tools(Arc::new(EventBus::new(16)), Arc::new(sandbox));

    // When: the reported command is dispatched by name through the standard executor.
    let result = executor
        .execute(
            &ToolExecutionContext {
                run_id: "shell-listing-regression".into(),
            },
            "shell",
            "listing",
            serde_json::json!({ "command": command, "timeout_ms": 5000 }),
        )
        .await
        .expect("shell returns an execution result");

    // Then: ls receives its flags, and shell operators are evaluated inside bwrap.
    assert!(!result.is_error, "{command}: {}", result.content);
    assert!(result.content.starts_with("exit_code: 0\n"));
    assert!(result.content.contains(".shell-listing-marker"));
    if command.starts_with("pwd") {
        assert_eq!(
            result.content.lines().nth(1),
            workspace.path().to_str(),
            "pwd must run in the sandbox workspace"
        );
    }
}

#[tokio::test]
#[ignore = "bwrap 実行環境が必要"]
async fn shell_runs_pwd_and_listing_when_command_contains_and_operator() {
    assert_shell_listing("pwd && ls -la").await;
}

#[tokio::test]
#[ignore = "bwrap 実行環境が必要"]
async fn shell_runs_listing_when_command_contains_arguments() {
    assert_shell_listing("ls -la").await;
}
