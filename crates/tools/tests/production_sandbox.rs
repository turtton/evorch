//! production 用 composition root（`with_production_sandbox`）経由で構築した
//! 実行器の e2e テスト。
//!
//! bwrap が実際に隔離を提供することを前提とするため、テストは
//! `#[ignore = "bwrap 実行環境が必要"]` で実環境でのみ実行される。

use std::sync::Arc;

use event_bus::EventBus;
use sandbox::BwrapConfig;
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
            serde_json::json!({ "command": "sh", "args": ["-c", "pwd"] }),
        )
        .await
        .expect("承認なしの Shell 実行は成功するはずです");

    let expected = workspace.path().display().to_string();
    assert!(!result.is_error);
    assert!(
        result.content.contains(&expected),
        "pwd の出力に作業パス {expected} が含まれない: {}",
        result.content
    );
}
