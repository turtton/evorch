//! 承認層と実行隔離層が独立して適用されることの統合テスト。

use std::sync::{Arc, Mutex};
use std::time::Duration;

use event_bus::{Event, EventBus, EventKind, ToolEvent};
use sandbox::{
    ApprovalGate, ApprovalMode, ApprovalPolicy, BwrapConfig, BwrapSandbox, CommandSpec,
    DirectSandbox, Sandbox, SandboxError, WrappedCommand,
};
use tools::{ToolExecutionContext, ToolExecutor};

#[derive(Clone)]
struct RecordingSandbox {
    specs: Arc<Mutex<Vec<CommandSpec>>>,
}

impl Sandbox for RecordingSandbox {
    fn wrap(&self, spec: CommandSpec) -> Result<WrappedCommand, SandboxError> {
        self.specs
            .lock()
            .expect("記録ロックを取得できるはずです")
            .push(spec.clone());
        DirectSandbox::new_unchecked().wrap(spec)
    }
}

fn set_approval(executor: &mut ToolExecutor, bus: Arc<EventBus>) -> tokio::task::JoinHandle<()> {
    executor
        .set_policy(ApprovalPolicy::standard(ApprovalMode::OnRequest))
        .set_approval_gate(ApprovalGate::new(Arc::clone(&bus), Duration::from_secs(1)));
    let mut receiver = bus.subscribe();
    tokio::spawn(async move {
        loop {
            let event = receiver.recv().await.expect("承認要求を受信できるはずです");
            if let EventKind::Tool(ToolEvent::ApprovalRequested { call_id, .. }) = event.kind {
                bus.emit(Event::new(ToolEvent::ApprovalResolved {
                    call_id,
                    approved: true,
                }));
                return;
            }
        }
    })
}

fn workspace_dir() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("tools-bwrap-")
        .tempdir_in(std::env::current_dir().expect("作業ディレクトリを取得できるはずです"))
        .expect("作業ツリー内に一時領域を作成できるはずです")
}

// Given: 承認が必要な Shell と記録サンドボックス / When: 承認後に実行 / Then: 実行仕様が必ずサンドボックス層へ渡る
#[tokio::test]
async fn approved_execution_still_routes_through_sandbox() {
    let bus = Arc::new(EventBus::new(16));
    let specs = Arc::new(Mutex::new(Vec::new()));
    let sandbox = RecordingSandbox {
        specs: Arc::clone(&specs),
    };
    let mut executor = ToolExecutor::with_standard_tools(Arc::clone(&bus), Arc::new(sandbox));
    let responder = set_approval(&mut executor, Arc::clone(&bus));

    executor
        .execute(
            &ToolExecutionContext {
                run_id: "run-1".to_string(),
            },
            "shell",
            "call-record",
            serde_json::json!({"command": "sh", "args": ["-c", "echo ok"]}),
        )
        .await
        .expect("承認後の Shell は成功するはずです");
    responder.await.expect("応答タスクが完了するはずです");

    let recorded = specs.lock().expect("記録ロックを取得できるはずです");
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].program, "sh");
}

// Given: 一時ワークスペースを bind した bwrap / When: 外側と内側へ書き込み / Then: 外側は失敗しホストに現れず内側だけ成功する
#[tokio::test]
#[ignore = "bwrap 実行環境が必要"]
async fn approved_bwrap_write_is_confined_to_workspace() {
    let workspace = workspace_dir();
    let outside = tempfile::tempdir().expect("外部領域を作成できるはずです");
    let sandbox = BwrapSandbox::detect(BwrapConfig::new(workspace.path().to_path_buf()))
        .expect("bwrap 実行環境が必要です");
    let bus = Arc::new(EventBus::new(32));
    let mut executor = ToolExecutor::with_standard_tools(Arc::clone(&bus), Arc::new(sandbox));
    let responder = set_approval(&mut executor, Arc::clone(&bus));
    let outside_file = outside.path().join("blocked.txt");

    let outside_result = executor
        .execute(
            &ToolExecutionContext {
                run_id: "run-1".to_string(),
            },
            "shell",
            "call-outside",
            serde_json::json!({
                "command": "sh",
                "args": ["-c", format!("printf blocked > {}", outside_file.display())]
            }),
        )
        .await
        .expect("プロセス自体は終了結果を返すはずです");
    responder
        .await
        .expect("外側書き込みの承認応答が完了するはずです");
    assert!(outside_result.is_error);
    assert!(!outside_file.exists());

    let responder = set_approval(&mut executor, Arc::clone(&bus));
    let inside_file = workspace.path().join("allowed.txt");
    let inside_result = executor
        .execute(
            &ToolExecutionContext {
                run_id: "run-1".to_string(),
            },
            "shell",
            "call-inside",
            serde_json::json!({
                "command": "sh",
                "args": ["-c", format!("printf allowed > {}", inside_file.display())]
            }),
        )
        .await
        .expect("内側書き込みは成功するはずです");
    responder
        .await
        .expect("内側書き込みの承認応答が完了するはずです");
    assert!(
        !inside_result.is_error,
        "作業領域内の書き込みが失敗しました: {}",
        inside_result.content
    );
    assert_eq!(
        std::fs::read_to_string(inside_file).expect("内側ファイルを読める"),
        "allowed"
    );
}

// Given: bwrap と対話 Shell / When: 承認後に PTY で echo を実行 / Then: 隔離内で正常終了して出力を返す
#[tokio::test]
#[ignore = "bwrap 実行環境が必要"]
async fn interactive_shell_runs_inside_bwrap() {
    let workspace = workspace_dir();
    let sandbox = BwrapSandbox::detect(BwrapConfig::new(workspace.path().to_path_buf()))
        .expect("bwrap 実行環境が必要です");
    let bus = Arc::new(EventBus::new(16));
    let mut executor = ToolExecutor::with_standard_tools(Arc::clone(&bus), Arc::new(sandbox));
    let responder = set_approval(&mut executor, Arc::clone(&bus));

    let result = executor
        .execute(
            &ToolExecutionContext {
                run_id: "run-1".to_string(),
            },
            "shell",
            "call-pty",
            serde_json::json!({
                "command": "sh",
                "args": ["-c", "echo ok"],
                "interactive": true
            }),
        )
        .await
        .expect("PTY 実行は成功するはずです");
    responder.await.expect("承認応答が完了するはずです");

    assert!(!result.is_error);
    assert!(result.content.contains("ok"));
}
