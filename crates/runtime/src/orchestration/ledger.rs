//! `OrchestratorEvent` を畳み込み、goal の決定的な現在状態を再構築する ledger。

use std::collections::{BTreeMap, BTreeSet};

use config::types::OrchestrationConfig;
use event_bus::{
    ApprovalDecision, CloseoutStep, GateEvidence, GateRejection, GateSnapshot, GoalReference,
    GoalStage, GoalState, InvalidationReason, MergeBinding, OrchestratorEvent, RunPurpose,
    StallSignal, SuppressReason,
};

use super::gate::{CiEvidence, CriteriaEvidence, GateInputs, PullRequestEvidence, ReviewEvidence};

/// 証跡 map のキー。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum EvidenceKind {
    /// Pull Request 証跡。
    PullRequest,
    /// CI 証跡。
    Ci,
    /// 受け入れ基準証跡。
    Criteria,
    /// reviewer 判定証跡。
    Review,
}

/// goal に紐付いた run の記録。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachedRun {
    /// run ID。
    pub run_id: String,
    /// 親 run ID。
    pub parent_run_id: Option<String>,
    /// role 名。
    pub role: String,
    /// run の目的。
    pub purpose: RunPurpose,
}

/// closeout ステップの最新記録。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloseoutRecord {
    /// closeout ステップ。
    pub step: CloseoutStep,
    /// 成功したか。
    pub ok: bool,
    /// artifact 参照。
    pub artifact_ref: Option<String>,
    /// 実行結果の概要。
    pub detail: String,
}

/// stall 検出記録。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StallRecord {
    /// 対象 run ID。
    pub run_id: String,
    /// 無活動時間。
    pub idle_ms: u64,
    /// stall 信号。
    pub signal: StallSignal,
}

/// nudge 送信記録。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NudgeRecord {
    /// 対象 run ID。
    pub run_id: String,
    /// 連続 nudge 番号。
    pub nudge_index: u32,
    /// メッセージ ID。
    pub message_id: String,
}

/// replay 可能な goal の完全スナップショット。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoalSnapshot {
    /// goal ID。
    pub goal_id: String,
    /// 永続化セッション ID。
    pub session_id: String,
    /// project ID。
    pub project_id: String,
    /// thread ID。
    pub thread_id: String,
    /// goal 本文。
    pub goal: String,
    /// goal の参照元。
    pub references: Vec<GoalReference>,
    /// goal の制約。
    pub constraints: Vec<String>,
    /// 対象リポジトリ。
    pub repo: String,
    /// マージ先ブランチ。
    pub base_ref: String,
    /// 最初の orchestrator run ID。
    pub root_run_id: String,
    /// 現在の orchestrator run ID。
    pub current_orchestrator_run_id: String,
    /// goal の状態。
    pub state: GoalState,
    /// goal のステージ。
    pub stage: GoalStage,
    /// 再起動後の runtime から切り離されているか。
    pub detached: bool,
    /// goal に紐付いた run。
    pub attached_runs: Vec<AttachedRun>,
    /// デリバラブルブランチ。
    pub deliverable_branch: Option<String>,
    /// ブランチを作成した run ID。
    pub deliverable_run_id: Option<String>,
    /// 種別ごとの最新 gate 証跡。
    pub evidence: BTreeMap<EvidenceKind, GateEvidence>,
    /// 最新の Pull Request 証跡。
    pub pull_request: Option<PullRequestEvidence>,
    /// 最新の CI 証跡。
    pub ci: Option<CiEvidence>,
    /// 最新の受け入れ基準証跡。
    pub criteria: Option<CriteriaEvidence>,
    /// 最新の reviewer 判定証跡。
    pub review: Option<ReviewEvidence>,
    /// 直近 finish 拒否理由。
    pub last_rejections: Vec<GateRejection>,
    /// 直近 finish 受理スナップショット。
    pub accepted_snapshot: Option<GateSnapshot>,
    /// idle continuation epoch。
    pub epoch: u64,
    /// dispatch 済み epoch。
    pub dispatched_epochs: BTreeSet<u64>,
    /// continuation 抑制の最新理由。
    pub continuation_suppressions: BTreeMap<u64, SuppressReason>,
    /// 開始済みレビューラウンドの最大値。
    pub review_rounds: u32,
    /// 修正が dispatch されたラウンドの最大値。
    pub repair_rounds: u32,
    /// stall 検出履歴。
    pub stalls: Vec<StallRecord>,
    /// nudge 履歴。
    pub nudges: Vec<NudgeRecord>,
    /// 発行済み承認バインディング。
    pub approvals_issued: Vec<MergeBinding>,
    /// 承認判定履歴。
    pub approval_resolutions: Vec<(String, ApprovalDecision)>,
    /// 承認無効化履歴。
    pub approval_invalidations: Vec<(String, InvalidationReason)>,
    /// 最新マージ結果。
    pub merge_result: Option<(u64, String, bool, String)>,
    /// closeout の各ステップに対する最新記録。
    pub closeout_steps: Vec<CloseoutRecord>,
}

/// runtime が使うオーケストレーション境界設定。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrchestrationSettings {
    /// レビューラウンド上限。
    pub max_review_rounds: u32,
    /// nudge 上限。
    pub max_nudges: u32,
    /// stall 判定秒数。
    pub stall_after_secs: u64,
    /// stall 観測間隔秒数。
    pub stall_check_secs: u64,
    /// in-flight ツールの stall 窓倍率。
    pub in_flight_tool_multiplier: u32,
    /// 連続ツールエラーしきい値。
    pub repeated_error_threshold: u32,
    /// continuation 上限。
    pub max_continuations: u32,
    /// CI poll 間隔秒数。
    pub ci_poll_secs: u64,
    /// CI timeout 秒数。
    pub ci_timeout_secs: u64,
}

impl Default for OrchestrationSettings {
    fn default() -> Self {
        Self::from(&OrchestrationConfig::default())
    }
}

impl From<&OrchestrationConfig> for OrchestrationSettings {
    fn from(config: &OrchestrationConfig) -> Self {
        Self {
            max_review_rounds: config.max_review_rounds,
            max_nudges: config.max_nudges,
            stall_after_secs: config.stall_after_secs,
            stall_check_secs: config.stall_check_secs,
            in_flight_tool_multiplier: config.in_flight_tool_multiplier,
            repeated_error_threshold: config.repeated_error_threshold,
            max_continuations: config.max_continuations,
            ci_poll_secs: config.ci_poll_secs,
            ci_timeout_secs: config.ci_timeout_secs,
        }
    }
}

/// ledger への不正なイベント適用。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LedgerError {
    /// イベントの goal ID が ledger と一致しない。
    #[error("event goal {actual} does not match ledger goal {expected}")]
    GoalMismatch {
        /// ledger 側の goal ID。
        expected: String,
        /// イベント側の goal ID。
        actual: String,
    },
    /// 状態遷移が許可されない。
    #[error("invalid goal state transition: {from:?} -> {to:?}")]
    InvalidTransition {
        /// 遷移元。
        from: GoalState,
        /// 遷移先。
        to: GoalState,
    },
    /// イベントが宣言した遷移元と現在状態が一致しない。
    #[error("goal state event expected {event_from:?}, current state is {current:?}")]
    StateConflict {
        /// 現在状態。
        current: GoalState,
        /// イベント記載の遷移元。
        event_from: GoalState,
    },
    /// ステージイベントの遷移元と現在ステージが一致しない。
    #[error("goal stage event expected {event_from:?}, current stage is {current:?}")]
    StageConflict {
        /// 現在ステージ。
        current: GoalStage,
        /// イベント記載の遷移元。
        event_from: GoalStage,
    },
    /// Complete に必要な closeout 条件を満たしていない。
    #[error("goal can complete only from active closeout with every closeout step successful")]
    CloseoutIncomplete,
    /// 作成済み ledger に GoalCreated を再適用した。
    #[error("goal creation event cannot be applied twice")]
    DuplicateCreation,
}

/// 1 goal のイベント畳み込み器。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoalLedger {
    snapshot: GoalSnapshot,
}

impl GoalLedger {
    /// 永続化済みスナップショットから ledger を復元する。
    pub fn from_snapshot(snapshot: GoalSnapshot) -> Self {
        Self { snapshot }
    }

    /// 再起動後の runtime から切り離された状態を設定する。
    pub fn set_detached(&mut self, detached: bool) {
        self.snapshot.detached = detached;
    }

    /// terminal / recovery により idle epoch を 1 つ進める。
    pub fn advance_epoch(&mut self) -> u64 {
        self.snapshot.epoch = self.snapshot.epoch.saturating_add(1);
        self.snapshot.epoch
    }
    /// `GoalCreated` から ledger を初期化する。
    ///
    /// # Panics
    /// `created` が `GoalCreated` でない場合は、呼び出し側の契約違反として panic する。
    pub fn new(created: &OrchestratorEvent) -> Self {
        let OrchestratorEvent::GoalCreated {
            goal_id,
            session_id,
            project_id,
            thread_id,
            goal,
            references,
            constraints,
            repo,
            base_ref,
            root_run_id,
        } = created
        else {
            panic!("GoalLedger::new requires GoalCreated");
        };
        Self {
            snapshot: GoalSnapshot {
                goal_id: goal_id.clone(),
                session_id: session_id.clone(),
                project_id: project_id.clone(),
                thread_id: thread_id.clone(),
                goal: goal.clone(),
                references: references.clone(),
                constraints: constraints.clone(),
                repo: repo.clone(),
                base_ref: base_ref.clone(),
                root_run_id: root_run_id.clone(),
                current_orchestrator_run_id: root_run_id.clone(),
                state: GoalState::Active,
                stage: GoalStage::Implementing,
                detached: false,
                attached_runs: vec![AttachedRun {
                    run_id: root_run_id.clone(),
                    parent_run_id: None,
                    role: "orchestrator".to_string(),
                    purpose: RunPurpose::Root,
                }],
                deliverable_branch: None,
                deliverable_run_id: None,
                evidence: BTreeMap::new(),
                pull_request: None,
                ci: None,
                criteria: None,
                review: None,
                last_rejections: Vec::new(),
                accepted_snapshot: None,
                epoch: 0,
                dispatched_epochs: BTreeSet::new(),
                continuation_suppressions: BTreeMap::new(),
                review_rounds: 0,
                repair_rounds: 0,
                stalls: Vec::new(),
                nudges: Vec::new(),
                approvals_issued: Vec::new(),
                approval_resolutions: Vec::new(),
                approval_invalidations: Vec::new(),
                merge_result: None,
                closeout_steps: Vec::new(),
            },
        }
    }

    /// 現在のスナップショットを返す。
    pub fn snapshot(&self) -> &GoalSnapshot {
        &self.snapshot
    }

    /// イベントを検証して現在状態へ適用する。
    ///
    /// # Errors
    /// goal ID、遷移元、状態遷移、または Complete 条件が不正なら失敗する。
    pub fn apply(&mut self, event: &OrchestratorEvent) -> Result<(), LedgerError> {
        if let Some(goal_id) = event_goal_id(event) {
            self.ensure_goal(goal_id)?;
        }
        match event {
            OrchestratorEvent::GoalCreated { .. } => Err(LedgerError::DuplicateCreation),
            OrchestratorEvent::GoalStateChanged { from, to, .. } => {
                if self.snapshot.state != *from {
                    return Err(LedgerError::StateConflict {
                        current: self.snapshot.state,
                        event_from: *from,
                    });
                }
                self.validate_transition(*to)?;
                if *from == GoalState::Paused && *to == GoalState::Active {
                    self.snapshot.epoch = self.snapshot.epoch.saturating_add(1);
                }
                self.snapshot.state = *to;
                Ok(())
            }
            OrchestratorEvent::GoalStageChanged { from, to, .. } => {
                if self.snapshot.stage != *from {
                    return Err(LedgerError::StageConflict {
                        current: self.snapshot.stage,
                        event_from: *from,
                    });
                }
                self.snapshot.stage = *to;
                Ok(())
            }
            OrchestratorEvent::RunAttached {
                run_id,
                parent_run_id,
                role,
                purpose,
                ..
            } => {
                if matches!(
                    purpose,
                    RunPurpose::Root
                        | RunPurpose::Continuation { .. }
                        | RunPurpose::Recovery { .. }
                ) {
                    self.snapshot.current_orchestrator_run_id = run_id.clone();
                }
                match purpose {
                    RunPurpose::Continuation { epoch } | RunPurpose::Recovery { epoch } => {
                        self.snapshot.epoch = self.snapshot.epoch.max(*epoch);
                    }
                    RunPurpose::Root
                    | RunPurpose::Implement
                    | RunPurpose::Repair { .. }
                    | RunPurpose::Review { .. } => {}
                }
                self.snapshot.attached_runs.push(AttachedRun {
                    run_id: run_id.clone(),
                    parent_run_id: parent_run_id.clone(),
                    role: role.clone(),
                    purpose: *purpose,
                });
                Ok(())
            }
            OrchestratorEvent::DeliverableBranchBound { branch, run_id, .. } => {
                self.snapshot.deliverable_branch = Some(branch.clone());
                self.snapshot.deliverable_run_id = Some(run_id.clone());
                Ok(())
            }
            OrchestratorEvent::EvidenceRecorded { evidence, .. } => {
                match evidence {
                    GateEvidence::PullRequest {
                        repo,
                        number,
                        url,
                        base_ref,
                        head_sha,
                    } => {
                        self.snapshot.pull_request = Some(PullRequestEvidence {
                            repo: repo.clone(),
                            number: *number,
                            url: url.clone(),
                            base_ref: base_ref.clone(),
                            head_sha: head_sha.clone(),
                        });
                    }
                    GateEvidence::Ci { head_sha, state } => {
                        self.snapshot.ci = Some(CiEvidence {
                            head_sha: head_sha.clone(),
                            state: state.clone(),
                        });
                    }
                    GateEvidence::Criteria {
                        head_sha,
                        reviewer_run_id,
                        round,
                        checklist,
                    } => {
                        self.snapshot.criteria = Some(CriteriaEvidence {
                            head_sha: head_sha.clone(),
                            reviewer_run_id: reviewer_run_id.clone(),
                            round: *round,
                            checklist: checklist.clone(),
                        });
                    }
                    GateEvidence::Review {
                        head_sha,
                        reviewer_run_id,
                        round,
                        verdict,
                    } => {
                        self.snapshot.review = Some(ReviewEvidence {
                            head_sha: head_sha.clone(),
                            reviewer_run_id: reviewer_run_id.clone(),
                            round: *round,
                            verdict: verdict.clone(),
                        });
                    }
                }
                self.snapshot
                    .evidence
                    .insert(evidence_kind(evidence), evidence.clone());
                Ok(())
            }
            OrchestratorEvent::FinishRejected { rejections, .. } => {
                self.snapshot.last_rejections = rejections.clone();
                Ok(())
            }
            OrchestratorEvent::FinishAccepted { snapshot, .. } => {
                self.snapshot.accepted_snapshot = Some(snapshot.clone());
                self.snapshot.last_rejections.clear();
                Ok(())
            }
            OrchestratorEvent::ContinuationDispatched { epoch, .. } => {
                self.snapshot.epoch = self.snapshot.epoch.max(*epoch);
                self.snapshot.dispatched_epochs.insert(*epoch);
                Ok(())
            }
            OrchestratorEvent::ContinuationSuppressed { epoch, reason, .. } => {
                self.snapshot.epoch = self.snapshot.epoch.max(*epoch);
                self.snapshot
                    .continuation_suppressions
                    .insert(*epoch, *reason);
                Ok(())
            }
            OrchestratorEvent::ReviewRoundStarted { round, .. } => {
                self.snapshot.review_rounds = self.snapshot.review_rounds.max(*round);
                Ok(())
            }
            OrchestratorEvent::RepairDispatched { round, .. } => {
                self.snapshot.repair_rounds = self.snapshot.repair_rounds.max(*round);
                Ok(())
            }
            OrchestratorEvent::StallDetected {
                run_id,
                idle_ms,
                signal,
                ..
            } => {
                self.snapshot.stalls.push(StallRecord {
                    run_id: run_id.clone(),
                    idle_ms: *idle_ms,
                    signal: *signal,
                });
                Ok(())
            }
            OrchestratorEvent::NudgeSent {
                run_id,
                nudge_index,
                message_id,
                ..
            } => {
                self.snapshot.nudges.push(NudgeRecord {
                    run_id: run_id.clone(),
                    nudge_index: *nudge_index,
                    message_id: message_id.clone(),
                });
                Ok(())
            }
            OrchestratorEvent::MergeApprovalRequested { binding, .. } => {
                self.snapshot.approvals_issued.push(binding.clone());
                Ok(())
            }
            OrchestratorEvent::MergeApprovalResolved {
                token_id, decision, ..
            } => {
                self.snapshot
                    .approval_resolutions
                    .push((token_id.clone(), decision.clone()));
                Ok(())
            }
            OrchestratorEvent::MergeApprovalInvalidated {
                token_id, reason, ..
            } => {
                self.snapshot
                    .approval_invalidations
                    .push((token_id.clone(), reason.clone()));
                Ok(())
            }
            OrchestratorEvent::MergeExecuted {
                pr_number,
                head_sha,
                ok,
                detail,
                ..
            } => {
                self.snapshot.merge_result =
                    Some((*pr_number, head_sha.clone(), *ok, detail.clone()));
                Ok(())
            }
            OrchestratorEvent::CloseoutStepRecorded {
                step,
                ok,
                artifact_ref,
                detail,
                ..
            } => {
                self.snapshot
                    .closeout_steps
                    .retain(|record| record.step != *step);
                self.snapshot.closeout_steps.push(CloseoutRecord {
                    step: *step,
                    ok: *ok,
                    artifact_ref: artifact_ref.clone(),
                    detail: detail.clone(),
                });
                Ok(())
            }
            OrchestratorEvent::ShellCommandDenied { .. } => Ok(()),
        }
    }

    /// 現在状態からの状態遷移イベントを生成する。
    ///
    /// # Errors
    /// 状態遷移または Complete 条件が不正なら失敗する。
    pub fn transition(
        &self,
        to: GoalState,
        reason: impl Into<String>,
    ) -> Result<OrchestratorEvent, LedgerError> {
        self.validate_transition(to)?;
        Ok(OrchestratorEvent::GoalStateChanged {
            goal_id: self.snapshot.goal_id.clone(),
            from: self.snapshot.state,
            to,
            reason: reason.into(),
        })
    }

    /// 複数 goal のイベント列を goal ID ごとの ledger へ replay する。
    pub fn replay<'a>(
        events: impl Iterator<Item = &'a OrchestratorEvent>,
    ) -> BTreeMap<String, GoalLedger> {
        let mut ledgers = BTreeMap::new();
        for event in events {
            if let OrchestratorEvent::GoalCreated { goal_id, .. } = event {
                ledgers.insert(goal_id.clone(), Self::new(event));
                continue;
            }
            let Some(goal_id) = event_goal_id(event) else {
                continue;
            };
            if let Some(ledger) = ledgers.get_mut(goal_id) {
                let _ = ledger.apply(event);
            }
        }
        ledgers
    }

    /// 現在の ledger から finish gate 入力を借用して構築する。
    pub fn gate_inputs<'a>(
        &'a self,
        current_head: Option<&'a str>,
        settings: OrchestrationSettings,
    ) -> GateInputs<'a> {
        GateInputs {
            expected_repo: &self.snapshot.repo,
            expected_base: &self.snapshot.base_ref,
            deliverable_branch: self.snapshot.deliverable_branch.as_deref(),
            current_head,
            pr: self.snapshot.pull_request.as_ref(),
            ci: self.snapshot.ci.as_ref(),
            criteria: self.snapshot.criteria.as_ref(),
            review: self.snapshot.review.as_ref(),
            review_rounds_used: self.snapshot.review_rounds,
            max_review_rounds: settings.max_review_rounds,
        }
    }

    /// continuation の純粋な dispatch predicate を評価する。
    pub fn can_dispatch_continuation(
        &self,
        gate_unmet: bool,
        orchestrator_terminal: bool,
        pipeline_busy: bool,
        max_continuations: u32,
    ) -> bool {
        self.snapshot.state == GoalState::Active
            && gate_unmet
            && orchestrator_terminal
            && !pipeline_busy
            && !self
                .snapshot
                .dispatched_epochs
                .contains(&self.snapshot.epoch)
            && self.snapshot.dispatched_epochs.len() < max_continuations as usize
    }

    fn ensure_goal(&self, actual: &str) -> Result<(), LedgerError> {
        if actual == self.snapshot.goal_id {
            Ok(())
        } else {
            Err(LedgerError::GoalMismatch {
                expected: self.snapshot.goal_id.clone(),
                actual: actual.to_string(),
            })
        }
    }

    fn validate_transition(&self, to: GoalState) -> Result<(), LedgerError> {
        let from = self.snapshot.state;
        let valid = matches!(
            (from, to),
            (GoalState::Active, GoalState::Paused)
                | (GoalState::Active, GoalState::Blocked)
                | (GoalState::Active, GoalState::Complete)
                | (GoalState::Active, GoalState::Cancelled)
                | (GoalState::Paused, GoalState::Active)
                | (GoalState::Paused, GoalState::Cancelled)
                | (GoalState::Blocked, GoalState::Active)
                | (GoalState::Blocked, GoalState::Cancelled)
        );
        if !valid {
            return Err(LedgerError::InvalidTransition { from, to });
        }
        if to == GoalState::Complete && !self.closeout_succeeded() {
            return Err(LedgerError::CloseoutIncomplete);
        }
        Ok(())
    }

    fn closeout_succeeded(&self) -> bool {
        self.snapshot.stage == GoalStage::Closeout
            && [
                CloseoutStep::WorkerClaim,
                CloseoutStep::ResultSummary,
                CloseoutStep::WorkerComplete,
            ]
            .iter()
            .all(|step| {
                self.snapshot
                    .closeout_steps
                    .iter()
                    .any(|record| record.step == *step && record.ok)
            })
    }
}

fn evidence_kind(evidence: &GateEvidence) -> EvidenceKind {
    match evidence {
        GateEvidence::PullRequest { .. } => EvidenceKind::PullRequest,
        GateEvidence::Ci { .. } => EvidenceKind::Ci,
        GateEvidence::Criteria { .. } => EvidenceKind::Criteria,
        GateEvidence::Review { .. } => EvidenceKind::Review,
    }
}

fn event_goal_id(event: &OrchestratorEvent) -> Option<&str> {
    match event {
        OrchestratorEvent::GoalCreated { goal_id, .. }
        | OrchestratorEvent::GoalStateChanged { goal_id, .. }
        | OrchestratorEvent::GoalStageChanged { goal_id, .. }
        | OrchestratorEvent::RunAttached { goal_id, .. }
        | OrchestratorEvent::DeliverableBranchBound { goal_id, .. }
        | OrchestratorEvent::EvidenceRecorded { goal_id, .. }
        | OrchestratorEvent::FinishRejected { goal_id, .. }
        | OrchestratorEvent::FinishAccepted { goal_id, .. }
        | OrchestratorEvent::ContinuationDispatched { goal_id, .. }
        | OrchestratorEvent::ContinuationSuppressed { goal_id, .. }
        | OrchestratorEvent::ReviewRoundStarted { goal_id, .. }
        | OrchestratorEvent::RepairDispatched { goal_id, .. }
        | OrchestratorEvent::StallDetected { goal_id, .. }
        | OrchestratorEvent::NudgeSent { goal_id, .. }
        | OrchestratorEvent::MergeApprovalRequested { goal_id, .. }
        | OrchestratorEvent::MergeApprovalResolved { goal_id, .. }
        | OrchestratorEvent::MergeApprovalInvalidated { goal_id, .. }
        | OrchestratorEvent::MergeExecuted { goal_id, .. }
        | OrchestratorEvent::CloseoutStepRecorded { goal_id, .. } => Some(goal_id),
        OrchestratorEvent::ShellCommandDenied { goal_id, .. } => goal_id.as_deref(),
    }
}
