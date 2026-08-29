//! shell ツールの統合テスト。
//!
//! 非対話モード（tokio::process）と対話モード（portable-pty）の双方について、
//! 出力キャプチャ・終了コード・タイムアウト・作業ディレクトリの契約を検証する。

use std::future::Future;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use sandbox::DirectSandbox;
use serde_json::json;
use tools::{Shell, Tool, ToolError};

fn shell() -> Shell {
    Shell::new(Arc::new(DirectSandbox))
}

/// PTY テスト全体の安全網。
///
/// Given: 任意の PTY テスト本体 / When: 30 秒で待機する / Then: 超過時はデッドロックを疑うメッセージで即座に失敗する
async fn with_pty_deadline<F: Future>(future: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(30), future)
        .await
        .unwrap_or_else(|_| {
            panic!("PTY テストが30秒以内に完了しませんでした（PTY デッドロックの疑いがあります）")
        })
}

// Given: stdout と stderr の両方に出力する sh コマンド / When: 非対話モードで実行 / Then: 終了コード 0 と両方の出力がセクション見出し付きで返る
#[tokio::test]
async fn shell_captures_stdout_stderr_and_exit_code() {
    let result = shell()
        .execute(json!({
            "command": "sh",
            "args": ["-c", "echo out; echo err >&2"]
        }))
        .await
        .expect("実行に成功するはずです");

    assert!(!result.is_error);
    assert!(result.content.contains("exit_code: 0"));
    assert!(result.content.contains("--- stdout ---"));
    assert!(result.content.contains("--- stderr ---"));
    assert!(result.content.contains("out"));
    assert!(result.content.contains("err"));
}

// Given: 終了コード 3 で終了する sh コマンド / When: 非対話モードで実行 / Then: ToolError ではなく is_error: true の ToolResult が返る
#[tokio::test]
async fn shell_nonzero_exit_is_error_result_not_tool_error() {
    let result = shell()
        .execute(json!({
            "command": "sh",
            "args": ["-c", "exit 3"]
        }))
        .await
        .expect("ツール自体は成功するはずです");

    assert!(result.is_error);
    assert!(result.content.contains("exit_code: 3"));
}

// Given: 存在しないバイナリ名 / When: 非対話モードで実行 / Then: 起動失敗として SpawnFailed が返る
#[tokio::test]
async fn shell_missing_binary_is_spawn_failed() {
    let error = shell()
        .execute(json!({
            "command": "definitely-not-a-real-binary-xyz"
        }))
        .await
        .expect_err("SpawnFailed が返るはずです");

    match error {
        ToolError::SpawnFailed { command, detail } => {
            assert_eq!(command, "definitely-not-a-real-binary-xyz");
            assert!(!detail.is_empty());
        }
        other => panic!("SpawnFailed を期待しましたが {other:?} が返りました"),
    }
}

// Given: 5 秒かかる sleep と 100ms の制限時間 / When: 非対話モードで実行 / Then: 子プロセスが殺されて Timeout エラーが返る
#[tokio::test]
async fn shell_timeout_kills_and_returns_timeout() {
    let error = shell()
        .execute(json!({
            "command": "sleep",
            "args": ["5"],
            "timeout_ms": 100
        }))
        .await
        .expect_err("Timeout が返るはずです");

    assert_eq!(error, ToolError::Timeout { timeout_ms: 100 });
}

// Given: 一時ディレクトリを cwd に指定した pwd / When: 非対話モードで実行 / Then: 出力にその一時ディレクトリのパスが含まれる
#[tokio::test]
async fn shell_respects_cwd() {
    let dir = tempfile::tempdir().expect("一時ディレクトリを作成できるはずです");

    let result = shell()
        .execute(json!({
            "command": "pwd",
            "cwd": dir.path()
        }))
        .await
        .expect("実行に成功するはずです");

    let expected = dir.path().to_str().expect("パスは UTF-8 であるはずです");
    assert!(result.content.contains(expected));
}

// Given: PTY 上で hello を出力する echo / When: 対話モードで実行 / Then: 出力に hello が含まれる（CRLF 変換があるため完全一致はしない）
#[tokio::test]
async fn shell_interactive_runs_in_pty_and_returns_output() {
    with_pty_deadline(async {
        let result = shell()
            .execute(json!({
                "command": "echo",
                "args": ["hello"],
                "interactive": true
            }))
            .await
            .expect("実行に成功するはずです");

        assert!(!result.is_error);
        assert!(result.content.contains("hello"));
    })
    .await;
}

// Given: PTY 上で終了コード 7 で終了する sh コマンド / When: 対話モードで実行 / Then: is_error: true かつ出力に終了コード 7 が含まれる
#[tokio::test]
async fn shell_interactive_reports_exit_code() {
    with_pty_deadline(async {
        let result = shell()
            .execute(json!({
                "command": "sh",
                "args": ["-c", "exit 7"],
                "interactive": true
            }))
            .await
            .expect("ツール自体は成功するはずです");

        assert!(result.is_error);
        assert!(result.content.contains("exit_code: 7"));
    })
    .await;
}

// Given: PTY 上の 5 秒かかる sleep と 100ms の制限時間 / When: 対話モードで実行 / Then: ChildKiller で殺されて Timeout エラーが返る
#[tokio::test]
async fn shell_interactive_timeout_kills_via_child_killer() {
    with_pty_deadline(async {
        let error = shell()
            .execute(json!({
                "command": "sleep",
                "args": ["5"],
                "interactive": true,
                "timeout_ms": 100
            }))
            .await
            .expect_err("Timeout が返るはずです");

        assert_eq!(error, ToolError::Timeout { timeout_ms: 100 });
    })
    .await;
}

// Given: 親テストプロセスだけに秘密環境変数がある / When: DirectSandbox 経由で sh を実行 / Then: PATH は利用できるが秘密値は子へ渡らない
#[test]
fn shell_direct_sandbox_scrubs_parent_secret_and_keeps_path() {
    const CHILD_FLAG: &str = "TOOLS_ENV_SCRUB_CHILD";
    if std::env::var_os(CHILD_FLAG).is_some() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("テスト用ランタイムを構築できるはずです");
        let result = runtime
            .block_on(shell().execute(json!({
                "command": "sh",
                "args": ["-c", "printf %s \"$FAKE_SECRET\""]
            })))
            .expect("PATH 経由で sh を起動できるはずです");
        assert!(result.content.contains("--- stdout ---\n\n--- stderr ---"));
        return;
    }

    let output = Command::new(std::env::current_exe().expect("テスト実行ファイルを取得できる"))
        .args([
            "--exact",
            "shell_direct_sandbox_scrubs_parent_secret_and_keeps_path",
            "--nocapture",
        ])
        .env(CHILD_FLAG, "1")
        .env("FAKE_SECRET", "漏えいしてはいけない値")
        .output()
        .expect("環境付き子テストを起動できるはずです");
    assert!(
        output.status.success(),
        "環境スクラブ子テストが失敗しました: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
