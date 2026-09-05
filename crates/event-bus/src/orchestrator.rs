//! オーケストレーションループ (goal lifecycle / gate / merge approval / closeout)
//! に関するイベントスキーマを定義します (issue #73)。
//!
//! wire 形状は既存カテゴリと同じ隣接タグの二重ネスト
//! (`{"kind":"Orchestrator","payload":{"kind":…,"payload":{…}}}`) であり、
//! 内部の補助 enum は `snake_case` で直列化される。

use serde::{Deserialize, Serialize};

/// goal のライフサイクル状態。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalState {
    /// 実行中。
    Active,
    /// 利用者により一時停止中。
    Paused,
    /// 境界条件によりBlocked (理由はイベントの reason を参照)。
    Blocked,
    /// 完了 (closeout 成功で到達する唯一の終端)。
    Complete,
    /// 利用者により取り消された終端。
    Cancelled,
}

/// goal の達成ステージ。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalStage {
    /// 実装を進めている。
    Implementing,
    /// デリバラブルを確定させている。
    Delivering,
    /// CI を待機している。
    AwaitingCi,
    /// レビュー中。
    Reviewing,
    /// レビュー指摘の修正中。
    Repairing,
    /// finish 判定の準備が整った。
    ReadyToFinish,
    /// マージ承認の判定待ち。
    AwaitingMergeApproval,
    /// マージ実行中。
    Merging,
    /// closeout 処理中。
    Closeout,
    /// goal としての作業が完了した。
    Done,
}

/// goal の参照元。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalReference {
    /// 参照種別 (`"packet"` または `"issue"`)。
    pub kind: String,
    /// 参照値 (packet id / issue 番号)。
    pub value: String,
}

/// run を goal に紐付けた際の目的。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunPurpose {
    /// goal の最初の orchestrator run。
    Root,
    /// idle 継続で起動された orchestrator run。
    Continuation {
        /// 継続エポック番号。
        epoch: u64,
    },
    /// 再起動後の復元で起動された orchestrator run。
    Recovery {
        /// 継続エポック番号。
        epoch: u64,
    },
    /// 実装 worker run。
    Implement,
    /// レビュー指摘の修正 worker run。
    Repair {
        /// 修正ラウンド番号。
        round: u32,
    },
    /// レビュー reviewer run。
    Review {
        /// レビューラウンド番号。
        round: u32,
    },
}

/// CI の観測状態。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CiState {
    /// 実行中または未完了。
    Pending,
    /// 成功。
    Green,
    /// 失敗。
    Failing {
        /// 失敗の概要 (ログ本文は含めない)。
        summary: String,
    },
}

/// 受け入れ基準の検査状態。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CriterionStatus {
    /// 満たした。
    Met,
    /// 満たしていない。
    Unmet,
    /// 検証不能 (未知は常に unmet 扱いで gate を通過しない)。
    Unknown,
}

/// 受け入れ基準 1 項目の検査結果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CriterionCheck {
    /// 基準の識別子。
    pub id: String,
    /// 検査状態。
    pub status: CriterionStatus,
    /// 判定の短い注記。
    pub note: String,
}

/// reviewer run の構造化判定。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewVerdict {
    /// 承認。
    Approve,
    /// 修正要求。
    RequestUpdate {
        /// 指摘一覧。
        findings: Vec<String>,
    },
}

/// finish gate に記録された証跠。
///
/// 証跡は head SHA で束縛され、鮮度判定は現在 head との一致のみで行う
/// (壁時計 TTL を持たない)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "payload")]
pub enum GateEvidence {
    /// PR の存在と位置づけ。
    PullRequest {
        /// リポジトリ (`owner/repo`)。
        repo: String,
        /// PR 番号。
        number: u64,
        /// PR の URL。
        url: String,
        /// マージ先ブランチ。
        base_ref: String,
        /// PR の head SHA。
        head_sha: String,
    },
    /// CI 状態。
    Ci {
        /// 観測対象の head SHA。
        head_sha: String,
        /// 観測された CI 状態。
        state: CiState,
    },
    /// 受け入れ基準の検査一式。
    Criteria {
        /// 検査対象の head SHA。
        head_sha: String,
        /// 検査した reviewer run の ID。
        reviewer_run_id: String,
        /// レビューラウンド番号。
        round: u32,
        /// 基準ごとの検査結果。
        checklist: Vec<CriterionCheck>,
    },
    /// reviewer の判定。
    Review {
        /// 判定対象の head SHA。
        head_sha: String,
        /// 判定した reviewer run の ID。
        reviewer_run_id: String,
        /// レビューラウンド番号。
        round: u32,
        /// 構造化判定。
        verdict: ReviewVerdict,
    },
}

/// finish gate が拒否した理由 (1 回の判定で該当すべてを返す)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "payload")]
pub enum GateRejection {
    /// goal が run に紐付いていない。
    NoGoalBound,
    /// デリバラブルブランチが確定していない。
    NoDeliverableBranch,
    /// PR が存在しない。
    NoPullRequest,
    /// PR のリポジトリが期待と異なる。
    PullRequestRepoMismatch {
        /// 期待値。
        expected: String,
        /// 実測値。
        actual: String,
    },
    /// PR のマージ先が期待と異なる。
    PullRequestBaseMismatch {
        /// 期待値。
        expected: String,
        /// 実測値。
        actual: String,
    },
    /// 証跡の head SHA が現在 head から遅延している。
    StaleHead {
        /// 遅延していた証跡種別。
        evidence: String,
        /// 証跡が束縛された head SHA。
        evidence_head: String,
        /// 現在の head SHA。
        current_head: String,
    },
    /// 最新 remote head を取得できず鮮度を証明できない (fail-closed)。
    RemoteHeadUnavailable {
        /// 取得失敗の詳細。
        detail: String,
    },
    /// 現在 head の CI 状態が存在しない。
    CiMissing {
        /// 現在の head SHA。
        head_sha: String,
    },
    /// 現在 head の CI が未完了。
    CiPending {
        /// 現在の head SHA。
        head_sha: String,
    },
    /// 現在 head の CI が失敗。
    CiFailing {
        /// 現在の head SHA。
        head_sha: String,
        /// 失敗の概要。
        summary: String,
    },
    /// 現在 head の受け入れ基準検査が存在しない。
    CriteriaUnverified {
        /// 現在の head SHA。
        head_sha: String,
    },
    /// 受け入れ基準に未達がある。
    CriteriaUnmet {
        /// 現在の head SHA。
        head_sha: String,
        /// 未達の基準識別子。
        ids: Vec<String>,
    },
    /// 現在 head の reviewer 判定が存在しない。
    ReviewMissing {
        /// 現在の head SHA。
        head_sha: String,
    },
    /// reviewer が修正要求した。
    ReviewRequestUpdate {
        /// 現在の head SHA。
        head_sha: String,
        /// レビューラウンド番号。
        round: u32,
    },
    /// reviewer 判定が現在 head より古い。
    ReviewStale {
        /// 判定が束縛された head SHA。
        reviewed_head: String,
        /// 現在の head SHA。
        current_head: String,
    },
    /// レビューラウンドの上限に達した。
    ReviewRoundsExhausted {
        /// 消費したラウンド数。
        rounds: u32,
    },
}

/// finish 受諾時点の gate 証跡スナップショット。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateSnapshot {
    /// リポジトリ (`owner/repo`)。
    pub repo: String,
    /// PR 番号。
    pub pr_number: u64,
    /// マージ先ブランチ。
    pub base_ref: String,
    /// PR の head SHA。
    pub head_sha: String,
    /// CI 状態。
    pub ci: CiState,
    /// 証跡に残る受け入れ基準検査のラウンド番号。
    pub criteria_round: u32,
    /// 証跡に残るレビューのラウンド番号。
    pub review_round: u32,
    /// 証跡に残る reviewer run の ID。
    pub reviewer_run_id: String,
}

/// マージ承認トークンとそれに束縛された gate スナップショット。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeBinding {
    /// 承認トークン識別子 (128-bit random)。
    pub token_id: String,
    /// リポジトリ (`owner/repo`)。
    pub repo: String,
    /// PR 番号。
    pub pr_number: u64,
    /// 承認対象の head SHA。
    pub head_sha: String,
    /// 承認時点の gate スナップショット。
    pub snapshot: GateSnapshot,
}

/// マージ承認の判定結果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    /// 承認。
    Approved,
    /// 却下。
    Rejected {
        /// 却下理由。
        reason: String,
    },
}

/// 承認トークンが無効化された理由。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "payload")]
pub enum InvalidationReason {
    /// head SHA が変化した。
    HeadChanged {
        /// 変化前の head SHA。
        from: String,
        /// 変化後の head SHA。
        to: String,
    },
    /// CI 状態が変化した。
    CiChanged,
    /// レビュー判定が変化した。
    ReviewChanged,
    /// トークンが消費済み。
    Consumed,
    /// 却下により無効化。
    Rejected,
    /// goal が Active でない。
    GoalNotActive,
}

/// stall 検出の信号。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StallSignal {
    /// 進捗が stall 窓を超えてない。
    NoProgress,
    /// 待機位相が stall 窓を超えている。
    WaitingTooLong,
    /// 連続ツールエラーがしきい値を超えた。
    RepeatedErrors {
        /// 連続エラー回数。
        count: u32,
    },
}

/// 継続ディスパッチを抑制した理由。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SuppressReason {
    /// 同一エポックで既にディスパッチ済み。
    Duplicate,
    /// goal が Paused。
    Paused,
    /// goal が Blocked。
    Blocked,
    /// goal が Complete。
    Complete,
    /// goal が Cancelled。
    Cancelled,
    /// 継続回数の上限に達した。
    LimitReached {
        /// 上限値。
        max: u32,
    },
    /// supervisor パイプラインが繁忙 (子 run または配信が進行中)。
    PipelineBusy,
}

/// closeout の 3 ステップ。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CloseoutStep {
    /// worker claim (`intent-cli worker claim`)。
    WorkerClaim,
    /// result summary (`intent-cli worker result-summary`)。
    ResultSummary,
    /// worker complete (`intent-cli worker complete`)。
    WorkerComplete,
}

/// オーケストレーションループに関するイベント。
///
/// 既存カテゴリと同じ隣接タグの二重ネストで wire に載る。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "payload")]
pub enum OrchestratorEvent {
    /// goal を登録した。
    GoalCreated {
        /// goal の永続識別子 (`goal-{wall_ms}-{seq}`)。
        goal_id: String,
        /// 記録先セッションの ID。
        session_id: String,
        /// プロジェクトの ID。
        project_id: String,
        /// スレッドの ID。
        thread_id: String,
        /// goal 本文。
        goal: String,
        /// 参照元一覧。
        references: Vec<GoalReference>,
        /// 制約一覧。
        constraints: Vec<String>,
        /// 対象リポジトリ (`owner/repo`)。
        repo: String,
        /// マージ先ブランチ。
        base_ref: String,
        /// 最初の orchestrator run の ID。
        root_run_id: String,
    },
    /// goal 状態が遷移した。
    GoalStateChanged {
        /// goal の ID。
        goal_id: String,
        /// 遷移前の状態。
        from: GoalState,
        /// 遷移後の状態。
        to: GoalState,
        /// 遷移理由。
        reason: String,
    },
    /// goal ステージが遷移した。
    GoalStageChanged {
        /// goal の ID。
        goal_id: String,
        /// 遷移前のステージ。
        from: GoalStage,
        /// 遷移後のステージ。
        to: GoalStage,
    },
    /// run を goal に紐付けた。
    RunAttached {
        /// goal の ID。
        goal_id: String,
        /// 紐付けた run の ID。
        run_id: String,
        /// 親 run の ID (ルートでは `None`)。
        parent_run_id: Option<String>,
        /// 実行 role。
        role: String,
        /// 紐付けの目的。
        purpose: RunPurpose,
    },
    /// デリバラブルブランチを確定させた。
    DeliverableBranchBound {
        /// goal の ID。
        goal_id: String,
        /// 確定したブランチ名。
        branch: String,
        /// ブランチを作った run の ID。
        run_id: String,
    },
    /// gate 証跡を記録した。
    EvidenceRecorded {
        /// goal の ID。
        goal_id: String,
        /// 記録した証跡。
        evidence: GateEvidence,
    },
    /// finish が gate に拒否された。
    FinishRejected {
        /// goal の ID。
        goal_id: String,
        /// finish を試みた run の ID。
        run_id: String,
        /// 拒否理由一覧。
        rejections: Vec<GateRejection>,
    },
    /// finish が gate に受諾された。
    FinishAccepted {
        /// goal の ID。
        goal_id: String,
        /// finish を試みた run の ID。
        run_id: String,
        /// 受諾時点のスナップショット。
        snapshot: GateSnapshot,
    },
    /// 継続 orchestrator run を起動した。
    ContinuationDispatched {
        /// goal の ID。
        goal_id: String,
        /// 継続エポック番号。
        epoch: u64,
        /// トリガーとなった run の ID。
        trigger_run_id: String,
        /// 起動した run の ID。
        new_run_id: String,
        /// 未達の gate 条件。
        unmet: Vec<GateRejection>,
    },
    /// 継続ディスパッチを抑制した。
    ContinuationSuppressed {
        /// goal の ID。
        goal_id: String,
        /// 継続エポック番号。
        epoch: u64,
        /// 抑制理由。
        reason: SuppressReason,
    },
    /// レビューラウンドを開始した。
    ReviewRoundStarted {
        /// goal の ID。
        goal_id: String,
        /// ラウンド番号。
        round: u32,
        /// 起動した reviewer run の ID。
        reviewer_run_id: String,
        /// レビュー対象の head SHA。
        head_sha: String,
    },
    /// 修正 worker run を起動した。
    RepairDispatched {
        /// goal の ID。
        goal_id: String,
        /// 修正ラウンド番号。
        round: u32,
        /// 起動した worker run の ID。
        worker_run_id: String,
        /// 修正対象の指摘一覧。
        findings: Vec<String>,
    },
    /// run の stall を検出した。
    StallDetected {
        /// goal の ID。
        goal_id: String,
        /// stall した run の ID。
        run_id: String,
        /// 前回進捗からの経過ミリ秒。
        idle_ms: u64,
        /// 検出信号。
        signal: StallSignal,
    },
    /// stall した run へ nudge を送った。
    NudgeSent {
        /// goal の ID。
        goal_id: String,
        /// nudge 先の run の ID。
        run_id: String,
        /// 連続 nudge 回数 (1 起点)。
        nudge_index: u32,
        /// 送信したメッセージの ID。
        message_id: String,
    },
    /// マージ承認を要求した。
    MergeApprovalRequested {
        /// goal の ID。
        goal_id: String,
        /// トークン束縛。
        binding: MergeBinding,
    },
    /// マージ承認が判定された。
    MergeApprovalResolved {
        /// goal の ID。
        goal_id: String,
        /// トークン識別子。
        token_id: String,
        /// 判定。
        decision: ApprovalDecision,
    },
    /// 未判定のマージ承認トークンを無効化した。
    MergeApprovalInvalidated {
        /// goal の ID。
        goal_id: String,
        /// トークン識別子。
        token_id: String,
        /// 無効化理由。
        reason: InvalidationReason,
    },
    /// マージを実行した。
    MergeExecuted {
        /// goal の ID。
        goal_id: String,
        /// マージした PR 番号。
        pr_number: u64,
        /// マージ対象の head SHA。
        head_sha: String,
        /// 成功したか。
        ok: bool,
        /// 実行結果の概要。
        detail: String,
    },
    /// closeout ステップを記録した。
    CloseoutStepRecorded {
        /// goal の ID。
        goal_id: String,
        /// 実行したステップ。
        step: CloseoutStep,
        /// 成功したか。
        ok: bool,
        /// 生成物の参照 (あれば)。
        artifact_ref: Option<String>,
        /// 実行結果の概要。
        detail: String,
    },
    /// shell コマンドが contract により拒否された。
    ShellCommandDenied {
        /// 対応する goal の ID (不明なら `None`)。
        goal_id: Option<String>,
        /// 対応する run の ID (不明なら `None`)。
        run_id: Option<String>,
        /// 拒否されたプログラム名。
        program: String,
        /// 拒否された引数一覧。
        args: Vec<String>,
        /// 拒否理由。
        reason: String,
    },
}

#[cfg(test)]
mod tests {
    use crate::event::Event;

    use super::*;

    fn sv<T: serde::Serialize>(value: &T) -> serde_json::Value {
        serde_json::to_value(value).expect("serialize schema type")
    }

    // Given: OrchestratorEvent の全バリアント
    // When: Event として JSON 往復する
    // Then: 値が保存され、外側タグ "Orchestrator" と内側隣接タグが期待どおりになる
    #[test]
    fn orchestrator_event_round_trips_every_variant() {
        let snapshot = GateSnapshot {
            repo: "turtton/evorch".into(),
            pr_number: 101,
            base_ref: "main".into(),
            head_sha: "a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0".into(),
            ci: CiState::Green,
            criteria_round: 1,
            review_round: 1,
            reviewer_run_id: "run-review-1".into(),
        };
        let binding = MergeBinding {
            token_id: "token-1".into(),
            repo: "turtton/evorch".into(),
            pr_number: 101,
            head_sha: "a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0".into(),
            snapshot: snapshot.clone(),
        };

        let cases: Vec<(&'static str, OrchestratorEvent)> = vec![
            (
                "GoalCreated",
                OrchestratorEvent::GoalCreated {
                    goal_id: "goal-1".into(),
                    session_id: "session-1".into(),
                    project_id: "evorch".into(),
                    thread_id: "thread-1".into(),
                    goal: "implement issue #73".into(),
                    references: vec![GoalReference {
                        kind: "issue".into(),
                        value: "73".into(),
                    }],
                    constraints: vec!["model only".into()],
                    repo: "turtton/evorch".into(),
                    base_ref: "main".into(),
                    root_run_id: "run-root-1".into(),
                },
            ),
            (
                "GoalStateChanged",
                OrchestratorEvent::GoalStateChanged {
                    goal_id: "goal-1".into(),
                    from: GoalState::Active,
                    to: GoalState::Blocked,
                    reason: "review rounds exhausted".into(),
                },
            ),
            (
                "GoalStageChanged",
                OrchestratorEvent::GoalStageChanged {
                    goal_id: "goal-1".into(),
                    from: GoalStage::AwaitingCi,
                    to: GoalStage::Reviewing,
                },
            ),
            (
                "RunAttached",
                OrchestratorEvent::RunAttached {
                    goal_id: "goal-1".into(),
                    run_id: "run-cont-1".into(),
                    parent_run_id: Some("run-root-1".into()),
                    role: "orchestrator".into(),
                    purpose: RunPurpose::Continuation { epoch: 2 },
                },
            ),
            (
                "DeliverableBranchBound",
                OrchestratorEvent::DeliverableBranchBound {
                    goal_id: "goal-1".into(),
                    branch: "evorch/task/run-1".into(),
                    run_id: "run-worker-1".into(),
                },
            ),
            (
                "EvidenceRecorded",
                OrchestratorEvent::EvidenceRecorded {
                    goal_id: "goal-1".into(),
                    evidence: GateEvidence::Ci {
                        head_sha: "a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0".into(),
                        state: CiState::Failing {
                            summary: "unit test failed".into(),
                        },
                    },
                },
            ),
            (
                "FinishRejected",
                OrchestratorEvent::FinishRejected {
                    goal_id: "goal-1".into(),
                    run_id: "run-root-1".into(),
                    rejections: vec![GateRejection::NoPullRequest],
                },
            ),
            (
                "FinishAccepted",
                OrchestratorEvent::FinishAccepted {
                    goal_id: "goal-1".into(),
                    run_id: "run-root-1".into(),
                    snapshot: snapshot.clone(),
                },
            ),
            (
                "ContinuationDispatched",
                OrchestratorEvent::ContinuationDispatched {
                    goal_id: "goal-1".into(),
                    epoch: 3,
                    trigger_run_id: "run-root-1".into(),
                    new_run_id: "run-cont-2".into(),
                    unmet: vec![GateRejection::CiPending {
                        head_sha: "a1".into(),
                    }],
                },
            ),
            (
                "ContinuationSuppressed",
                OrchestratorEvent::ContinuationSuppressed {
                    goal_id: "goal-1".into(),
                    epoch: 4,
                    reason: SuppressReason::PipelineBusy,
                },
            ),
            (
                "ReviewRoundStarted",
                OrchestratorEvent::ReviewRoundStarted {
                    goal_id: "goal-1".into(),
                    round: 2,
                    reviewer_run_id: "run-review-2".into(),
                    head_sha: "a2".into(),
                },
            ),
            (
                "RepairDispatched",
                OrchestratorEvent::RepairDispatched {
                    goal_id: "goal-1".into(),
                    round: 1,
                    worker_run_id: "run-repair-1".into(),
                    findings: vec!["missing test".into()],
                },
            ),
            (
                "StallDetected",
                OrchestratorEvent::StallDetected {
                    goal_id: "goal-1".into(),
                    run_id: "run-cont-2".into(),
                    idle_ms: 600_000,
                    signal: StallSignal::RepeatedErrors { count: 5 },
                },
            ),
            (
                "NudgeSent",
                OrchestratorEvent::NudgeSent {
                    goal_id: "goal-1".into(),
                    run_id: "run-cont-2".into(),
                    nudge_index: 2,
                    message_id: "message-1".into(),
                },
            ),
            (
                "MergeApprovalRequested",
                OrchestratorEvent::MergeApprovalRequested {
                    goal_id: "goal-1".into(),
                    binding,
                },
            ),
            (
                "MergeApprovalResolved",
                OrchestratorEvent::MergeApprovalResolved {
                    goal_id: "goal-1".into(),
                    token_id: "token-1".into(),
                    decision: ApprovalDecision::Rejected {
                        reason: "stale head".into(),
                    },
                },
            ),
            (
                "MergeApprovalInvalidated",
                OrchestratorEvent::MergeApprovalInvalidated {
                    goal_id: "goal-1".into(),
                    token_id: "token-1".into(),
                    reason: InvalidationReason::HeadChanged {
                        from: "a1".into(),
                        to: "a2".into(),
                    },
                },
            ),
            (
                "MergeExecuted",
                OrchestratorEvent::MergeExecuted {
                    goal_id: "goal-1".into(),
                    pr_number: 101,
                    head_sha: "a2".into(),
                    ok: true,
                    detail: "squash merged".into(),
                },
            ),
            (
                "CloseoutStepRecorded",
                OrchestratorEvent::CloseoutStepRecorded {
                    goal_id: "goal-1".into(),
                    step: CloseoutStep::ResultSummary,
                    ok: false,
                    artifact_ref: Some("summary.md".into()),
                    detail: "worker did not claim".into(),
                },
            ),
            (
                "ShellCommandDenied",
                OrchestratorEvent::ShellCommandDenied {
                    goal_id: None,
                    run_id: None,
                    program: "git".into(),
                    args: vec!["push".into(), "origin".into()],
                    reason: "denied by contract".into(),
                },
            ),
        ];

        for (inner_tag, event) in cases {
            let outer = Event::new(event);
            let json = serde_json::to_string(&outer).expect("serialize Event");
            let restored: Event = serde_json::from_str(&json).expect("deserialize Event");
            assert_eq!(outer, restored, "round-trip mismatch: {inner_tag}");

            let value: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
            assert_eq!(
                value["kind"]["kind"], "Orchestrator",
                "outer tag mismatch: {inner_tag}"
            );
            assert_eq!(
                value["kind"]["payload"]["kind"], inner_tag,
                "inner tag mismatch"
            );
        }
    }

    // Given: GateRejection のデータ付きバリアントと unit バリアント
    // When: JSON 値へシリアライズする
    // Then: 隣接タグ形状 {"kind", "payload"} を保ち、unit バリアントは tag のみで往復する
    #[test]
    fn gate_rejection_serializes_with_adjacent_tags() {
        let stale = GateRejection::StaleHead {
            evidence: "pull_request".into(),
            evidence_head: "a1".into(),
            current_head: "a2".into(),
        };
        let value = serde_json::to_value(&stale).expect("serialize StaleHead");
        assert_eq!(
            value,
            serde_json::json!({
                "kind": "StaleHead",
                "payload": {
                    "evidence": "pull_request",
                    "evidence_head": "a1",
                    "current_head": "a2"
                }
            })
        );
        let restored: GateRejection = serde_json::from_value(value).expect("deserialize StaleHead");
        assert_eq!(stale, restored);

        let unit = GateRejection::NoGoalBound;
        let value = serde_json::to_value(&unit).expect("serialize NoGoalBound");
        assert_eq!(value, serde_json::json!({ "kind": "NoGoalBound" }));
        let restored: GateRejection =
            serde_json::from_value(value).expect("deserialize NoGoalBound");
        assert_eq!(unit, restored);

        let exhausted = GateRejection::ReviewRoundsExhausted { rounds: 3 };
        let json = serde_json::to_string(&exhausted).expect("serialize ReviewRoundsExhausted");
        let restored: GateRejection =
            serde_json::from_str(&json).expect("deserialize ReviewRoundsExhausted");
        assert_eq!(exhausted, restored);

        let unavailable = GateRejection::RemoteHeadUnavailable {
            detail: "remote head could not be fetched".into(),
        };
        let json = serde_json::to_string(&unavailable).expect("serialize RemoteHeadUnavailable");
        let restored: GateRejection =
            serde_json::from_str(&json).expect("deserialize RemoteHeadUnavailable");
        assert_eq!(unavailable, restored);
    }

    // Given: スキーマ補助型の全バリアント
    // When: JSON 値へシリアライズして復元する
    // Then: snake_case / 隣接タグの pinned shape が保たれ、値が往復する
    #[test]
    fn orchestrator_sub_schema_types_round_trip() {
        let checks = vec![CriterionCheck {
            id: "ac1".into(),
            status: CriterionStatus::Unmet,
            note: "not implemented".into(),
        }];
        let round_trips: Vec<(serde_json::Value, serde_json::Value)> = vec![
            // snake_case の unit enum は文字列へ直列化される。
            (sv(&GoalState::Active), serde_json::json!("active")),
            (sv(&GoalState::Paused), serde_json::json!("paused")),
            (sv(&GoalState::Blocked), serde_json::json!("blocked")),
            (sv(&GoalState::Complete), serde_json::json!("complete")),
            (sv(&GoalState::Cancelled), serde_json::json!("cancelled")),
            (
                sv(&GoalStage::Implementing),
                serde_json::json!("implementing"),
            ),
            (sv(&GoalStage::Delivering), serde_json::json!("delivering")),
            (sv(&GoalStage::AwaitingCi), serde_json::json!("awaiting_ci")),
            (sv(&GoalStage::Reviewing), serde_json::json!("reviewing")),
            (sv(&GoalStage::Repairing), serde_json::json!("repairing")),
            (
                sv(&GoalStage::ReadyToFinish),
                serde_json::json!("ready_to_finish"),
            ),
            (
                sv(&GoalStage::AwaitingMergeApproval),
                serde_json::json!("awaiting_merge_approval"),
            ),
            (sv(&GoalStage::Merging), serde_json::json!("merging")),
            (sv(&GoalStage::Closeout), serde_json::json!("closeout")),
            (sv(&GoalStage::Done), serde_json::json!("done")),
            (sv(&RunPurpose::Root), serde_json::json!("root")),
            (
                sv(&RunPurpose::Continuation { epoch: 2 }),
                serde_json::json!({ "continuation": { "epoch": 2 } }),
            ),
            (
                sv(&RunPurpose::Recovery { epoch: 7 }),
                serde_json::json!({ "recovery": { "epoch": 7 } }),
            ),
            (sv(&RunPurpose::Implement), serde_json::json!("implement")),
            (
                sv(&RunPurpose::Repair { round: 2 }),
                serde_json::json!({ "repair": { "round": 2 } }),
            ),
            (
                sv(&RunPurpose::Review { round: 1 }),
                serde_json::json!({ "review": { "round": 1 } }),
            ),
            (sv(&CiState::Pending), serde_json::json!("pending")),
            (sv(&CiState::Green), serde_json::json!("green")),
            (
                sv(&CiState::Failing {
                    summary: "boom".into(),
                }),
                serde_json::json!({ "failing": { "summary": "boom" } }),
            ),
            (sv(&CriterionStatus::Met), serde_json::json!("met")),
            (sv(&CriterionStatus::Unmet), serde_json::json!("unmet")),
            (sv(&CriterionStatus::Unknown), serde_json::json!("unknown")),
            (
                sv(&CriterionCheck {
                    id: "ac1".into(),
                    status: CriterionStatus::Unmet,
                    note: "not implemented".into(),
                }),
                serde_json::json!({
                    "id": "ac1",
                    "status": "unmet",
                    "note": "not implemented"
                }),
            ),
            (sv(&ReviewVerdict::Approve), serde_json::json!("approve")),
            (
                sv(&ReviewVerdict::RequestUpdate {
                    findings: vec!["x".into()],
                }),
                serde_json::json!({ "request_update": { "findings": ["x"] } }),
            ),
            (
                sv(&ApprovalDecision::Approved),
                serde_json::json!("approved"),
            ),
            (
                sv(&StallSignal::NoProgress),
                serde_json::json!("no_progress"),
            ),
            (
                sv(&StallSignal::WaitingTooLong),
                serde_json::json!("waiting_too_long"),
            ),
            (
                sv(&StallSignal::RepeatedErrors { count: 3 }),
                serde_json::json!({ "repeated_errors": { "count": 3 } }),
            ),
            (
                sv(&SuppressReason::Duplicate),
                serde_json::json!("duplicate"),
            ),
            (sv(&SuppressReason::Paused), serde_json::json!("paused")),
            (sv(&SuppressReason::Blocked), serde_json::json!("blocked")),
            (sv(&SuppressReason::Complete), serde_json::json!("complete")),
            (
                sv(&SuppressReason::Cancelled),
                serde_json::json!("cancelled"),
            ),
            (
                sv(&SuppressReason::LimitReached { max: 8 }),
                serde_json::json!({ "limit_reached": { "max": 8 } }),
            ),
            (
                sv(&SuppressReason::PipelineBusy),
                serde_json::json!("pipeline_busy"),
            ),
            (
                sv(&CloseoutStep::WorkerClaim),
                serde_json::json!("worker_claim"),
            ),
            (
                sv(&CloseoutStep::ResultSummary),
                serde_json::json!("result_summary"),
            ),
            (
                sv(&CloseoutStep::WorkerComplete),
                serde_json::json!("worker_complete"),
            ),
            // 隣接タグ enum の pinned shape。
            (
                sv(&GateEvidence::PullRequest {
                    repo: "turtton/evorch".into(),
                    number: 101,
                    url: "https://github.com/turtton/evorch/pull/101".into(),
                    base_ref: "main".into(),
                    head_sha: "a1b2c3d4".into(),
                }),
                serde_json::json!({
                    "kind": "PullRequest",
                    "payload": {
                        "repo": "turtton/evorch",
                        "number": 101,
                        "url": "https://github.com/turtton/evorch/pull/101",
                        "base_ref": "main",
                        "head_sha": "a1b2c3d4"
                    }
                }),
            ),
            (
                sv(&GateEvidence::Ci {
                    head_sha: "h".into(),
                    state: CiState::Pending,
                }),
                serde_json::json!({
                    "kind": "Ci",
                    "payload": { "head_sha": "h", "state": "pending" }
                }),
            ),
            (
                sv(&GateEvidence::Criteria {
                    head_sha: "h".into(),
                    reviewer_run_id: "r".into(),
                    round: 1,
                    checklist: checks,
                }),
                serde_json::json!({
                    "kind": "Criteria",
                    "payload": {
                        "head_sha": "h",
                        "reviewer_run_id": "r",
                        "round": 1,
                        "checklist": [
                            { "id": "ac1", "status": "unmet", "note": "not implemented" }
                        ]
                    }
                }),
            ),
            (
                sv(&GateEvidence::Review {
                    head_sha: "h".into(),
                    reviewer_run_id: "r".into(),
                    round: 1,
                    verdict: ReviewVerdict::Approve,
                }),
                serde_json::json!({
                    "kind": "Review",
                    "payload": {
                        "head_sha": "h",
                        "reviewer_run_id": "r",
                        "round": 1,
                        "verdict": "approve"
                    }
                }),
            ),
            (
                sv(&InvalidationReason::HeadChanged {
                    from: "a1".into(),
                    to: "a2".into(),
                }),
                serde_json::json!({
                    "kind": "HeadChanged",
                    "payload": { "from": "a1", "to": "a2" }
                }),
            ),
            (
                sv(&InvalidationReason::CiChanged),
                serde_json::json!({ "kind": "CiChanged" }),
            ),
            (
                sv(&InvalidationReason::ReviewChanged),
                serde_json::json!({ "kind": "ReviewChanged" }),
            ),
            (
                sv(&InvalidationReason::Consumed),
                serde_json::json!({ "kind": "Consumed" }),
            ),
            (
                sv(&InvalidationReason::Rejected),
                serde_json::json!({ "kind": "Rejected" }),
            ),
            (
                sv(&InvalidationReason::GoalNotActive),
                serde_json::json!({ "kind": "GoalNotActive" }),
            ),
        ];

        for (value, expected) in round_trips {
            assert_eq!(value, expected, "pinned shape mismatch");
        }
    }
}
