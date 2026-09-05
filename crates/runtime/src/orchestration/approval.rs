//! SHA-bound merge approval token のライフサイクル。

use std::collections::HashMap;

use event_bus::{GateSnapshot, InvalidationReason, MergeBinding};

use super::types::ApprovedMerge;

/// 外部へ公開する承認トークン状態。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalStatus {
    /// 利用者の判断待ち。
    Pending,
    /// matching snapshot に対して承認済み。
    Approved,
    /// 理由付きで却下済み。
    Rejected {
        /// 利用者が記録した却下理由。
        reason: String,
    },
    /// 証跡変更または消費で無効化済み。
    Invalidated(InvalidationReason),
}

/// 承認操作の失敗。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ApprovalError {
    /// token ID が存在しない。
    #[error("unknown merge approval token")]
    Unknown,
    /// token が pending ではない。
    #[error("merge approval token is not pending")]
    NotPending,
    /// token が指定理由で無効化済み。
    #[error("merge approval token is invalidated: {0:?}")]
    Invalidated(InvalidationReason),
    /// 現在証跡が binding と一致せず、自動無効化した。
    #[error("merge approval snapshot mismatch: {0:?}")]
    SnapshotMismatch(InvalidationReason),
}

#[derive(Debug, Clone)]
struct PendingApproval {
    goal_id: String,
    binding: MergeBinding,
    state: ApprovalStatus,
}

/// goal ごとの pending merge approval token を管理する。
#[derive(Debug, Default)]
pub struct MergeApprovals {
    pending: HashMap<String, PendingApproval>,
}

impl MergeApprovals {
    /// OS CSPRNG を使って gate snapshot に束縛した token を発行する。
    ///
    /// # Errors
    /// operating system の乱数源が利用できない場合に失敗する。
    pub fn issue_random(
        &mut self,
        goal_id: impl Into<String>,
        snapshot: GateSnapshot,
    ) -> Result<MergeBinding, getrandom::Error> {
        let bytes = random_token_bytes()?;
        Ok(self.issue_with_bytes(goal_id.into(), snapshot, bytes))
    }

    /// gate snapshot に束縛した 128-bit token を発行する。
    pub fn issue(
        &mut self,
        goal_id: impl Into<String>,
        snapshot: GateSnapshot,
        rng: &dyn Fn() -> [u8; 16],
    ) -> MergeBinding {
        self.issue_with_bytes(goal_id.into(), snapshot, rng())
    }

    fn issue_with_bytes(
        &mut self,
        goal_id: String,
        snapshot: GateSnapshot,
        bytes: [u8; 16],
    ) -> MergeBinding {
        let token_id = hex_token(bytes);
        let binding = MergeBinding {
            token_id: token_id.clone(),
            repo: snapshot.repo.clone(),
            pr_number: snapshot.pr_number,
            head_sha: snapshot.head_sha.clone(),
            snapshot,
        };
        self.pending.insert(
            token_id,
            PendingApproval {
                goal_id,
                binding: binding.clone(),
                state: ApprovalStatus::Pending,
            },
        );
        binding
    }

    /// binding と現在 snapshot が完全一致する場合だけ承認済みにする。
    ///
    /// # Errors
    /// token が unknown/non-pending、または証跡が変化している場合に失敗する。
    pub fn approve(&mut self, token_id: &str, current: &GateSnapshot) -> Result<(), ApprovalError> {
        let approval = self
            .pending
            .get_mut(token_id)
            .ok_or(ApprovalError::Unknown)?;
        require_pending(&approval.state)?;
        if approval.binding.snapshot != *current {
            let reason = mismatch_reason(&approval.binding.snapshot, current);
            approval.state = ApprovalStatus::Invalidated(reason.clone());
            return Err(ApprovalError::SnapshotMismatch(reason));
        }
        approval.state = ApprovalStatus::Approved;
        Ok(())
    }

    /// 承認済み token を一度だけ型付き merge capability へ変換する。
    ///
    /// # Errors
    /// token が unknown、未承認、却下済み、または無効化済みなら失敗する。
    pub fn consume(&mut self, token_id: &str) -> Result<ApprovedMerge, ApprovalError> {
        let approval = self
            .pending
            .get_mut(token_id)
            .ok_or(ApprovalError::Unknown)?;
        match &approval.state {
            ApprovalStatus::Approved => {
                approval.state = ApprovalStatus::Invalidated(InvalidationReason::Consumed);
                Ok(ApprovedMerge {
                    binding: approval.binding.clone(),
                })
            }
            ApprovalStatus::Pending => Err(ApprovalError::NotPending),
            ApprovalStatus::Rejected { .. } => {
                Err(ApprovalError::Invalidated(InvalidationReason::Rejected))
            }
            ApprovalStatus::Invalidated(reason) => Err(ApprovalError::Invalidated(reason.clone())),
        }
    }

    /// pending token を理由付きで却下する。
    ///
    /// # Errors
    /// token が unknown または pending でない場合に失敗する。
    pub fn reject(
        &mut self,
        token_id: &str,
        reason: impl Into<String>,
    ) -> Result<(), ApprovalError> {
        let approval = self
            .pending
            .get_mut(token_id)
            .ok_or(ApprovalError::Unknown)?;
        require_pending(&approval.state)?;
        approval.state = ApprovalStatus::Rejected {
            reason: reason.into(),
        };
        Ok(())
    }

    /// goal に属する pending/approved token を一括無効化する。
    pub fn invalidate_for_goal(&mut self, goal_id: &str, reason: InvalidationReason) {
        for approval in self
            .pending
            .values_mut()
            .filter(|item| item.goal_id == goal_id)
        {
            if matches!(
                approval.state,
                ApprovalStatus::Pending | ApprovalStatus::Approved
            ) {
                approval.state = ApprovalStatus::Invalidated(reason.clone());
            }
        }
    }

    /// token の現在状態を返す。
    pub fn status(&self, token_id: &str) -> Option<ApprovalStatus> {
        self.pending
            .get(token_id)
            .map(|approval| approval.state.clone())
    }
}

fn require_pending(state: &ApprovalStatus) -> Result<(), ApprovalError> {
    match state {
        ApprovalStatus::Pending => Ok(()),
        ApprovalStatus::Approved | ApprovalStatus::Rejected { .. } => {
            Err(ApprovalError::NotPending)
        }
        ApprovalStatus::Invalidated(reason) => Err(ApprovalError::Invalidated(reason.clone())),
    }
}

fn mismatch_reason(bound: &GateSnapshot, current: &GateSnapshot) -> InvalidationReason {
    if bound.head_sha != current.head_sha {
        InvalidationReason::HeadChanged {
            from: bound.head_sha.clone(),
            to: current.head_sha.clone(),
        }
    } else if bound.ci != current.ci {
        InvalidationReason::CiChanged
    } else {
        InvalidationReason::ReviewChanged
    }
}

fn hex_token(bytes: [u8; 16]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut token = String::with_capacity(32);
    for byte in bytes {
        token.push(char::from(HEX[usize::from(byte >> 4)]));
        token.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    token
}

/// OS の CSPRNG から 128-bit token source を返す。
///
/// # Errors
/// operating system の乱数源が利用できない場合に失敗する。
pub fn random_token_bytes() -> Result<[u8; 16], getrandom::Error> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes)?;
    Ok(bytes)
}
