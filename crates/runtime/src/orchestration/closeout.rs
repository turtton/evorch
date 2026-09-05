//! merge 成功後だけ実行する ordered closeout sequence。

use event_bus::CloseoutStep;

use super::delivery::DeliveryPort;

const CLOSEOUT_STEPS: [CloseoutStep; 3] = [
    CloseoutStep::WorkerClaim,
    CloseoutStep::ResultSummary,
    CloseoutStep::WorkerComplete,
];

/// closeout 全体の終端状態。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseoutStatus {
    /// merge 未成功のため未実行。
    NotMerged,
    /// 全ステップ成功。
    Complete,
    /// いずれかのステップが失敗し、後続を停止。
    Blocked,
}

/// 永続イベントへ変換可能な 1 ステップの実行記録。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloseoutRecord {
    /// 実行したステップ。
    pub step: CloseoutStep,
    /// 成否。
    pub ok: bool,
    /// 成功時の artifact 参照。
    pub artifact_ref: Option<String>,
    /// 成功または失敗の診断。
    pub detail: String,
}

/// closeout sequence の結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloseoutResult {
    /// sequence の状態。
    pub status: CloseoutStatus,
    /// 実際に試行したステップのみを順序付きで保持する。
    pub records: Vec<CloseoutRecord>,
}

/// merge 成功後に claim → summary → complete を順番に実行する。
pub async fn run_closeout(
    delivery: &dyn DeliveryPort,
    goal_id: &str,
    merge_ok: bool,
) -> CloseoutResult {
    if !merge_ok {
        return CloseoutResult {
            status: CloseoutStatus::NotMerged,
            records: Vec::new(),
        };
    }

    let mut records = Vec::with_capacity(CLOSEOUT_STEPS.len());
    for step in CLOSEOUT_STEPS {
        match delivery.closeout_step(goal_id, step).await {
            Ok(artifact_ref) => records.push(CloseoutRecord {
                step,
                ok: true,
                artifact_ref,
                detail: "ok".to_string(),
            }),
            Err(error) => {
                records.push(CloseoutRecord {
                    step,
                    ok: false,
                    artifact_ref: None,
                    detail: error.to_string(),
                });
                return CloseoutResult {
                    status: CloseoutStatus::Blocked,
                    records,
                };
            }
        }
    }
    CloseoutResult {
        status: CloseoutStatus::Complete,
        records,
    }
}
