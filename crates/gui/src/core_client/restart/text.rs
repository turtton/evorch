//! The desktop's wording: the reconnect lines are English, the rest still Japanese
//! (PR18). What is distinguished comes from
//! `workbench_client::restart::view`; only the sentences are the desktop's.
use std::time::Duration;
use workbench_client::{
    ClientError,
    restart::{
        reconnect::Stage,
        view::{FailureKind, classify},
    },
};
use workbench_protocol::{
    ids::RunId,
    namespaces::lifecycle::{
        AdminOperationState, AdminOutcome, AdminReceipt, RestartCancelReason, RestartPhase,
    },
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
        FailureKind::ActionRequired => match error {
            ClientError::Server(e) => format!(
                "先に対応が必要なため受け付けられませんでした（{}）。何も変更していません",
                e.message
            ),
            other => other.to_string(),
        },
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

pub fn approval_blocker(run: &RunId) -> String {
    format!("run {run} が承認待ちです。承認か拒否をするか run を停止してください")
}

pub fn cancel_reason(reason: RestartCancelReason) -> String {
    match reason {
        RestartCancelReason::ApprovalPending { pending } => format!(
            "承認待ちが {pending} 件あるため再起動を取り消しました。承認か拒否をするか run を停止してから、もう一度要求してください"
        ),
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
            "Restarting: connecting only (no new core is started). Waiting for the successor core",
            120,
        ),
        Stage::ReadyToReconnect => ("The successor core is ready. Rebuilding this window", 30),
    };
    format!("{what} ({}s of {total}s left)", remaining.as_secs())
}

pub fn expired(stage: Stage) -> &'static str {
    match stage {
        Stage::RestartingToReady => {
            "Could not connect to the successor core within 120 seconds. The core was not stopped and no other core was started"
        }
        Stage::ReadyToReconnect => {
            "The successor core is ready, but this window could not be rebuilt within 30 seconds. Nothing was resent"
        }
    }
}

pub fn undisposed(count: u64) -> String {
    format!(
        "再起動できません：前回の再起動から未処分のグラフが {count} 件残っています。管理 CLI で `evorch core graphs list`（隔離中のものは `evorch core quarantine list`）を実行し、解放・再開・破棄してからもう一度操作してください。何も変更していません"
    )
}

pub fn outcome_unknown() -> String {
    "結果が不明です。要求は再送していません。再接続後に操作の状態を読み直して確認します".into()
}
