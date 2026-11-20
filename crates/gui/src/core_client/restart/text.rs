//! Japanese wording. What is distinguished comes from
//! `workbench_client::restart::view`; only the sentences are the desktop's.
use std::time::Duration;
use workbench_client::{
    ClientError,
    restart::{
        reconnect::Stage,
        view::{FailureKind, classify},
    },
};
use workbench_protocol::namespaces::lifecycle::{
    AdminOperationState, AdminOutcome, AdminReceipt, RestartPhase,
};

pub fn phase(phase: RestartPhase) -> &'static str {
    match phase {
        RestartPhase::Running => "稼働中",
        RestartPhase::Requested => "要求済み",
        RestartPhase::PreparingDrains => "準備中",
        RestartPhase::WaitingSafePoint => "安全点を待機中",
        RestartPhase::Sealing => "封印中",
        RestartPhase::Sealed => "封印済み",
        RestartPhase::Capturing => "状態を取得中",
        RestartPhase::Finalizing => "最終処理中",
        RestartPhase::Cancelling => "取消中",
        RestartPhase::FailedClosed => "失敗して停止中",
        RestartPhase::ShuttingDown => "停止処理中",
    }
}

pub fn failure(error: &ClientError) -> String {
    match classify(error) {
        FailureKind::Stale => {
            "競合：確認した対象または状態がすでに古くなっています。何も変更していません。読み直してからもう一度判断してください".into()
        }
        FailureKind::Busy => "コアが混み合っています。何も変更していません".into(),
        FailureKind::Unsupported => {
            "このコアは安全な再起動に対応していません。何も変更していません".into()
        }
        FailureKind::Denied => "この操作は許可されていません。何も変更していません".into(),
        FailureKind::Unknown => {
            "結果が不明です。要求は再送していません。操作を読み直して確認してください".into()
        }
        FailureKind::Other => match error {
            ClientError::Server(e) => e.message.clone(),
            other => other.to_string(),
        },
    }
}

pub fn receipt(receipt: &AdminReceipt) -> Vec<String> {
    let state = match receipt.state {
        AdminOperationState::Pending => "受付済み（未開始）",
        AdminOperationState::Started => "実行中",
        AdminOperationState::Completed => "完了",
        AdminOperationState::Unknown => "状態不明",
    };
    let outcome = match receipt.outcome {
        AdminOutcome::Pending => "未確定",
        AdminOutcome::Succeeded => "成功",
        AdminOutcome::Failed => "失敗",
        AdminOutcome::Cancelled => "取消",
        AdminOutcome::Conflict => "競合（対象が変わりました）",
        AdminOutcome::Unknown => "不明",
    };
    let mut lines = vec![format!(
        "操作 {}：{state}、結果 {outcome}",
        receipt.operation_id.as_str()
    )];
    if receipt.audit_warning {
        lines.push("警告：この操作の監査記録を完全には保存できませんでした".into());
    }
    lines
}

pub fn reconnect(stage: Stage, remaining: Duration) -> String {
    let (what, total) = match stage {
        Stage::RestartingToReady => (
            "再起動中：接続のみ試行しています（新しいコアは起動しません）。後継コアを待っています",
            120,
        ),
        Stage::ReadyToReconnect => ("後継コアが準備できました。この画面を再構築しています", 30),
    };
    format!("{what}（{total}秒中 残り{}秒）", remaining.as_secs())
}

pub fn expired(stage: Stage) -> &'static str {
    match stage {
        Stage::RestartingToReady => {
            "120秒以内に後継コアへ接続できませんでした。コアは終了させておらず、別のコアも起動していません"
        }
        Stage::ReadyToReconnect => {
            "後継コアは準備できましたが、30秒以内にこの画面を再構築できませんでした。何も再送していません"
        }
    }
}
