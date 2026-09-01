//! [`ToolExecutor`] の統合テスト。
//!
//! ツール実行の窓口として、開始・完了イベントの発行、引数スキーマ検証、
//! 制御マーカのエスケープ（ADR 0008）を検証する。

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use event_bus::{Event, EventBus, EventKind, EventReceiver, ToolEvent};
use sandbox::DirectSandbox;
use tempfile::tempdir;
use tools::{Permissions, Read, Tool, ToolError, ToolExecutor, ToolResult};

/// テスト用フィクスチャ（バス・実行器・受信者）を生成する。
///
/// 受信者は `execute` より先に登録するため、実行中に発行された全イベントを
/// 順序どおり受け取れる。
fn setup_executor() -> (Arc<EventBus>, ToolExecutor, EventReceiver) {
    let bus = Arc::new(EventBus::new(16));
    let receiver = bus.subscribe();
    let executor = ToolExecutor::with_standard_tools(
        Arc::clone(&bus),
        Arc::new(DirectSandbox::new_unchecked()),
    );
    (bus, executor, receiver)
}

/// イベントから [`ToolEvent`] を取り出す。
fn tool_event(event: &Event) -> &ToolEvent {
    let EventKind::Tool(tool_event) = &event.kind else {
        panic!("Tool イベントを期待しましたが {:?} でした", event.kind);
    };
    tool_event
}

/// 一時ディレクトリをカレントにして git サブコマンドを実行する（フィクスチャ用）。
///
/// ユーザーの git 設定を読まないよう `GIT_CONFIG_GLOBAL` / `GIT_CONFIG_SYSTEM`
/// を無効化する。
fn run_git(dir: &Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?} が失敗しました: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// コミットを含まない空の git リポジトリを作成する（フィクスチャ用）。
fn init_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    run_git(dir.path(), &["init"]);
    dir
}

// Given: 標準ツールを登録した実行器と一時ファイル / When: read を実行 / Then: ToolStarted → ToolCompleted(is_error=false) の順で両イベントのペイロードが受信できる
#[tokio::test]
async fn executor_emits_started_then_completed_with_payload() {
    let (_bus, executor, mut receiver) = setup_executor();
    let dir = tempdir().expect("一時ディレクトリの作成に失敗");
    let path = dir.path().join("sample.txt");
    std::fs::write(&path, "hello\n").expect("テストファイルの書き込みに失敗");

    let result = executor
        .execute(
            "read",
            "call-1",
            serde_json::json!({ "path": path.display().to_string() }),
        )
        .await
        .expect("既存ファイルの読み取りは成功する");

    assert!(!result.is_error);

    let first = receiver.recv().await.expect("1 件目のイベントを受信できる");
    assert_eq!(
        tool_event(&first),
        &ToolEvent::ToolStarted {
            tool_name: "read".to_string(),
            call_id: "call-1".to_string(),
        }
    );
    let second = receiver.recv().await.expect("2 件目のイベントを受信できる");
    assert_eq!(
        tool_event(&second),
        &ToolEvent::ToolCompleted {
            tool_name: "read".to_string(),
            call_id: "call-1".to_string(),
            is_error: false,
            detail: None,
        }
    );
}

// Given: 標準ツールを登録した実行器 / When: 存在しないツール名で実行 / Then: UnknownTool が返り、事後のセンチネル送信が最初の受信イベントになる（事前発行なしの証明）
#[tokio::test]
async fn executor_unknown_tool_is_error_without_events() {
    let (bus, executor, mut receiver) = setup_executor();

    let error = executor
        .execute("nonexistent", "call-x", serde_json::json!({}))
        .await
        .expect_err("未登録ツールはエラーになる");

    assert_eq!(
        error,
        ToolError::UnknownTool {
            name: "nonexistent".to_string(),
        }
    );

    // センチネルを送信し、受信の先頭がそれであることで実行中の発行がなかったことを証明する。
    bus.emit(Event::new(ToolEvent::ToolStarted {
        tool_name: "sentinel".to_string(),
        call_id: "sentinel".to_string(),
    }));

    let first = receiver.recv().await.expect("センチネルを受信できる");
    assert_eq!(
        tool_event(&first),
        &ToolEvent::ToolStarted {
            tool_name: "sentinel".to_string(),
            call_id: "sentinel".to_string(),
        }
    );
}

// Given: 標準ツールを登録した実行器 / When: read を必須 path 欠落と未知プロパティ付きでそれぞれ実行 / Then: いずれも InvalidArgs で失敗し、ToolStarted と ToolCompleted(is_error=true) が受信できる
#[tokio::test]
async fn executor_invalid_args_emit_completed_error() {
    let (_bus, executor, mut receiver) = setup_executor();
    let dir = tempdir().expect("一時ディレクトリの作成に失敗");
    let path = dir.path().join("sample.txt");
    std::fs::write(&path, "hello\n").expect("テストファイルの書き込みに失敗");

    // 必須プロパティ path の欠落
    let error = executor
        .execute("read", "call-missing", serde_json::json!({}))
        .await
        .expect_err("path 欠落は InvalidArgs になる");
    let ToolError::InvalidArgs { detail } = error else {
        panic!("InvalidArgs を期待しましたが {error:?} でした");
    };
    assert!(!detail.is_empty(), "違反の詳細が空: {detail}");

    let started = receiver.recv().await.expect("1 件目のイベントを受信できる");
    assert_eq!(
        tool_event(&started),
        &ToolEvent::ToolStarted {
            tool_name: "read".to_string(),
            call_id: "call-missing".to_string(),
        }
    );
    let completed = receiver.recv().await.expect("2 件目のイベントを受信できる");
    assert_eq!(
        tool_event(&completed),
        &ToolEvent::ToolCompleted {
            tool_name: "read".to_string(),
            call_id: "call-missing".to_string(),
            is_error: true,
            detail: None,
        }
    );

    // スキーマ未定義の未知プロパティ
    let error = executor
        .execute(
            "read",
            "call-extra",
            serde_json::json!({ "path": path.display().to_string(), "extra": 1 }),
        )
        .await
        .expect_err("未知プロパティは InvalidArgs になる");
    assert!(
        matches!(error, ToolError::InvalidArgs { .. }),
        "実際のエラー: {error:?}"
    );

    let started = receiver.recv().await.expect("3 件目のイベントを受信できる");
    assert_eq!(
        tool_event(&started),
        &ToolEvent::ToolStarted {
            tool_name: "read".to_string(),
            call_id: "call-extra".to_string(),
        }
    );
    let completed = receiver.recv().await.expect("4 件目のイベントを受信できる");
    assert_eq!(
        tool_event(&completed),
        &ToolEvent::ToolCompleted {
            tool_name: "read".to_string(),
            call_id: "call-extra".to_string(),
            is_error: true,
            detail: None,
        }
    );
}

// Given: 標準ツールを登録した実行器 / When: 存在しないパスで read を実行 / Then: PathNotFound が伝播し、ToolStarted と ToolCompleted(is_error=true) が受信できる
#[tokio::test]
async fn executor_tool_error_emits_completed_error() {
    let (_bus, executor, mut receiver) = setup_executor();
    let dir = tempdir().expect("一時ディレクトリの作成に失敗");
    let missing = dir.path().join("missing.txt");

    let error = executor
        .execute(
            "read",
            "call-1",
            serde_json::json!({ "path": missing.display().to_string() }),
        )
        .await
        .expect_err("存在しないパスはエラーになる");

    assert_eq!(
        error,
        ToolError::PathNotFound {
            path: missing.display().to_string(),
        }
    );

    let started = receiver.recv().await.expect("1 件目のイベントを受信できる");
    assert_eq!(
        tool_event(&started),
        &ToolEvent::ToolStarted {
            tool_name: "read".to_string(),
            call_id: "call-1".to_string(),
        }
    );
    let completed = receiver.recv().await.expect("2 件目のイベントを受信できる");
    assert_eq!(
        tool_event(&completed),
        &ToolEvent::ToolCompleted {
            tool_name: "read".to_string(),
            call_id: "call-1".to_string(),
            is_error: true,
            detail: None,
        }
    );
}

// Given: 生の <system-reminder> を含むファイル / When: read を実行 / Then: 結果本文はエスケープ済みで生マーカーを含まない
#[tokio::test]
async fn executor_escapes_control_markers_in_result() {
    let (_bus, executor, _receiver) = setup_executor();
    let dir = tempdir().expect("一時ディレクトリの作成に失敗");
    let path = dir.path().join("marker.txt");
    std::fs::write(&path, "before <system-reminder> after\n")
        .expect("テストファイルの書き込みに失敗");

    let result = executor
        .execute(
            "read",
            "call-1",
            serde_json::json!({ "path": path.display().to_string() }),
        )
        .await
        .expect("既存ファイルの読み取りは成功する");

    assert!(
        result.content.contains("<\\system-reminder>"),
        "エスケープ済みマーカーが含まれない: {}",
        result.content
    );
    // エスケープ後は `\` が挿入されるため、生マーカーは連続部分列として現れない。
    assert!(
        !result.content.contains("<system-reminder>"),
        "生マーカーが残っている: {}",
        result.content
    );
}

// Given: 標準ツールを登録した実行器 / When: 終了コード 3 で終わるコマンドを shell で実行 / Then: Ok かつ is_error=true の結果になり、ToolCompleted(is_error=true) が受信できる
#[tokio::test]
async fn executor_shell_nonzero_exit_flags_is_error_in_event() {
    let (_bus, executor, mut receiver) = setup_executor();

    let result = executor
        .execute(
            "shell",
            "call-1",
            serde_json::json!({ "command": "sh", "args": ["-c", "exit 3"] }),
        )
        .await
        .expect("非ゼロ終了はエラー値ではなく結果として返る");

    assert!(result.is_error);
    assert!(
        result.content.contains("exit_code: 3"),
        "終了コードが本文に含まれない: {}",
        result.content
    );

    let started = receiver.recv().await.expect("1 件目のイベントを受信できる");
    assert_eq!(
        tool_event(&started),
        &ToolEvent::ToolStarted {
            tool_name: "shell".to_string(),
            call_id: "call-1".to_string(),
        }
    );
    let completed = receiver.recv().await.expect("2 件目のイベントを受信できる");
    assert_eq!(
        tool_event(&completed),
        &ToolEvent::ToolCompleted {
            tool_name: "shell".to_string(),
            call_id: "call-1".to_string(),
            is_error: true,
            detail: None,
        }
    );
}

// Given: 標準ツールを登録した実行器と各ツール用のフィクスチャ / When: 5 ツールそれぞれを最小引数で実行 / Then: すべて正常終了する
#[tokio::test]
async fn executor_with_standard_tools_registers_five() {
    let (_bus, executor, _receiver) = setup_executor();

    // read / grep 用のファイルと edit 用の書き込み先
    let dir = tempdir().expect("一時ディレクトリの作成に失敗");
    let file = dir.path().join("sample.txt");
    std::fs::write(&file, "needle\n").expect("テストファイルの書き込みに失敗");
    let file_path = file.display().to_string();
    let edit_target = dir.path().join("edit_target.txt");
    let edit_target_path = edit_target.display().to_string();

    // git_diff 用の実リポジトリ（ステージ済み変更つき）
    let repo = init_repo();
    let repo_file = repo.path().join("sample.txt");
    std::fs::write(&repo_file, "original\n").expect("テストファイルの書き込みに失敗");
    run_git(repo.path(), &["add", "sample.txt"]);
    std::fs::write(&repo_file, "modified\n").expect("テストファイルの書き込みに失敗");

    let cases = [
        ("read", serde_json::json!({ "path": file_path.clone() })),
        (
            "grep",
            serde_json::json!({ "pattern": "needle", "path": file_path }),
        ),
        (
            "edit",
            serde_json::json!({ "path": edit_target_path, "new_string": "hello" }),
        ),
        (
            "shell",
            serde_json::json!({ "command": "sh", "args": ["-c", "true"] }),
        ),
        (
            "git_diff",
            serde_json::json!({ "cwd": repo.path().to_string_lossy() }),
        ),
    ];

    for (name, args) in cases {
        let result = executor
            .execute(name, "call-1", args)
            .await
            .unwrap_or_else(|error| panic!("{name} の実行に失敗しました: {error}"));
        assert!(
            !result.is_error,
            "{name} が異常終了しました: {}",
            result.content
        );
    }
}

/// detail メタデータを添えて正常終了するテスト用ツール。
struct DetailTool {
    detail: serde_json::Value,
}

#[async_trait]
impl Tool for DetailTool {
    fn name(&self) -> &'static str {
        "detail_tool"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object", "additionalProperties": false })
    }

    fn permissions(&self) -> Permissions {
        Permissions::read_only()
    }

    async fn execute(&self, _args: serde_json::Value) -> Result<ToolResult, ToolError> {
        Ok(ToolResult::success("本文").with_detail(self.detail.clone()))
    }
}

// Given: detail を添えるテスト用ツール / When: Executor 経由で実行 / Then: ToolCompleted イベントがツールが返した detail を運ぶ
#[tokio::test]
async fn executor_emits_tool_completed_with_detail() {
    let bus = Arc::new(EventBus::new(16));
    let mut receiver = bus.subscribe();
    let mut executor = ToolExecutor::new(bus);
    executor
        .register(Arc::new(DetailTool {
            detail: serde_json::json!({ "request_id": "req-1", "query": "evorch" }),
        }))
        .expect("テストツールを登録できるはずです");

    executor
        .execute("detail_tool", "call-1", serde_json::json!({}))
        .await
        .expect("テストツールは成功する");

    let started = receiver.recv().await.expect("1 件目のイベントを受信できる");
    assert_eq!(
        tool_event(&started),
        &ToolEvent::ToolStarted {
            tool_name: "detail_tool".to_string(),
            call_id: "call-1".to_string(),
        }
    );
    let completed = receiver.recv().await.expect("2 件目のイベントを受信できる");
    assert_eq!(
        tool_event(&completed),
        &ToolEvent::ToolCompleted {
            tool_name: "detail_tool".to_string(),
            call_id: "call-1".to_string(),
            is_error: false,
            detail: Some(serde_json::json!({
                "request_id": "req-1",
                "query": "evorch"
            })),
        }
    );
}

/// スキーマがコンパイルできないテスト用ツール。
struct BrokenTool;

#[async_trait]
impl Tool for BrokenTool {
    fn name(&self) -> &'static str {
        "broken"
    }

    fn schema(&self) -> serde_json::Value {
        // type キーに文字列以外を指定した不正なスキーマ。
        serde_json::json!({ "type": 42 })
    }

    fn permissions(&self) -> Permissions {
        Permissions::read_only()
    }

    async fn execute(&self, _args: serde_json::Value) -> Result<ToolResult, ToolError> {
        Ok(ToolResult::success("登録に失敗するため実行されない"))
    }
}

// Given: 不正なスキーマを持つツール / When: register を呼び出す / Then: InvalidSchema で拒否される
#[tokio::test]
async fn executor_register_rejects_uncompilable_schema() {
    let bus = Arc::new(EventBus::new(16));
    let mut executor = ToolExecutor::new(bus);

    let error = executor
        .register(Arc::new(BrokenTool))
        .expect_err("不正なスキーマの登録は失敗する");

    let ToolError::InvalidSchema { tool_name, detail } = error else {
        panic!("InvalidSchema を期待しましたが {error:?} でした");
    };
    assert_eq!(tool_name, "broken");
    assert!(!detail.is_empty(), "違反の詳細が空");
}

// Given: 生の <system-reminder> を含むファイル / When: Read を直接実行（Executor 経由ではない）/ Then: 本文は生マーカーをそのまま含む（エスケープは Executor の責務）
#[tokio::test]
async fn direct_tool_call_does_not_escape_markers() {
    let dir = tempdir().expect("一時ディレクトリの作成に失敗");
    let path = dir.path().join("marker.txt");
    std::fs::write(&path, "before <system-reminder> after\n")
        .expect("テストファイルの書き込みに失敗");

    let result = Read
        .execute(serde_json::json!({ "path": path.display().to_string() }))
        .await
        .expect("既存ファイルの読み取りは成功する");

    assert!(
        result.content.contains("<system-reminder>"),
        "生マーカーが保持されていない: {}",
        result.content
    );
}
