//! What a person is told. Surfaces differ in wording (the desktop is Japanese)
//! but not in what they distinguish, so the distinctions live here: which
//! failure is a stale target, whether the audit trail is in trouble, where the
//! reconnect stands and by when it must succeed.
use super::reconnect::Stage;
use crate::{ClientError, TransportError};
use std::time::Duration;
use workbench_protocol::{
    ErrorCode,
    namespaces::lifecycle::{
        AdminOperationState, AdminOutcome, AdminReceipt, RestartBlockerKind, RestartCancelReason,
        RestartPhase,
        restart::{RestartManagementStatus, RestartTargets, ShellBlockers},
    },
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    /// The target, epoch or revision shown is no longer the core's. Read again.
    Stale,
    Busy,
    /// The core does not offer restart (or not on this platform).
    Unsupported,
    Denied,
    /// The command may have run. It was not sent again; ask the core.
    Unknown,
    Other,
}

pub fn classify(error: &ClientError) -> FailureKind {
    match error {
        ClientError::Server(e) => match e.code {
            ErrorCode::Conflict | ErrorCode::StaleRevision | ErrorCode::StaleEpoch => {
                FailureKind::Stale
            }
            ErrorCode::Busy | ErrorCode::LimitExceeded | ErrorCode::Draining => FailureKind::Busy,
            ErrorCode::UnsupportedCapability | ErrorCode::UnsupportedVersion => {
                FailureKind::Unsupported
            }
            ErrorCode::PermissionDenied => FailureKind::Denied,
            ErrorCode::Internal => FailureKind::Unknown,
            _ => FailureKind::Other,
        },
        ClientError::Transport(TransportError::OutcomeUnknown { .. }) => FailureKind::Unknown,
        _ => FailureKind::Other,
    }
}

pub fn describe_failure(error: &ClientError) -> String {
    let detail = match error {
        ClientError::Server(e) => e.message.clone(),
        other => other.to_string(),
    };
    match classify(error) {
        FailureKind::Stale => format!(
            "conflict: the target or state you confirmed is no longer current ({detail}); nothing was changed. Read the targets and status again and decide again"
        ),
        FailureKind::Busy => format!("the core is busy ({detail}); nothing was changed"),
        FailureKind::Unsupported => {
            format!("this core does not offer safe restart ({detail}); nothing was changed")
        }
        FailureKind::Denied => format!("not permitted ({detail}); nothing was changed"),
        FailureKind::Unknown => format!(
            "the outcome is unknown ({detail}); the request was not sent again. Read the status or the operation before doing it again"
        ),
        FailureKind::Other => detail,
    }
}

pub fn phase_name(phase: RestartPhase) -> &'static str {
    match phase {
        RestartPhase::Running => "running",
        RestartPhase::Requested => "requested",
        RestartPhase::PreparingDrains => "preparing",
        RestartPhase::WaitingSafePoint => "waiting for a safe point",
        RestartPhase::Sealing => "sealing",
        RestartPhase::Sealed => "sealed",
        RestartPhase::Capturing => "capturing",
        RestartPhase::Finalizing => "finalizing",
        RestartPhase::Cancelling => "cancelling",
        RestartPhase::FailedClosed => "failed closed",
        RestartPhase::ShuttingDown => "shutting down",
    }
}

fn blocker_kind_name(kind: RestartBlockerKind) -> &'static str {
    match kind {
        RestartBlockerKind::InFlightAdmission => "admission in flight",
        RestartBlockerKind::AcceptedResumePending => "accepted resume pending",
        RestartBlockerKind::AwaitingSafePoint => "awaiting a safe point",
        RestartBlockerKind::ShellRunning => "shell running",
        RestartBlockerKind::ShellResultUnobserved => "shell result not observed",
        RestartBlockerKind::UnconfirmedRunJs => "run_js not confirmed",
        RestartBlockerKind::UnfinishedToolBatch => "tool batch unfinished",
        RestartBlockerKind::ApprovalPending => {
            "approval pending (approve or deny it, or stop the run)"
        }
    }
}

fn cancel_reason(reason: RestartCancelReason) -> String {
    match reason {
        RestartCancelReason::ApprovalPending { pending } => format!(
            "{pending} approval(s) were pending, and a restart would lose them; approve or deny the pending approval(s), or stop the run, then retry"
        ),
    }
}

pub fn describe_targets(targets: &RestartTargets) -> Vec<String> {
    let mut lines = vec![format!(
        "core epoch {}, targets revision {}",
        targets.core_epoch.get(),
        targets.revision.get()
    )];
    for target in &targets.targets {
        lines.push(format!(
            "  {} (revision {})",
            target.id.as_str(),
            target.revision.get()
        ));
    }
    if targets.truncated {
        lines.push("  (the list was cut off)".into());
    }
    lines
}

pub fn describe_status(status: &RestartManagementStatus) -> Vec<String> {
    let s = &status.status;
    let mut lines = vec![format!(
        "restart: {} (revision {}, core epoch {})",
        phase_name(s.phase),
        s.revision,
        status.core_epoch.get()
    )];
    if let Some(id) = &s.restart_id {
        lines.push(format!("  restart id {id}"));
    }
    if s.blocker_total > 0 {
        lines.push(format!(
            "  {} blocker(s){}",
            s.blocker_total,
            if s.blocker_omitted > 0 {
                format!(", {} not listed", s.blocker_omitted)
            } else {
                String::new()
            }
        ));
        for blocker in &s.blockers {
            lines.push(format!(
                "    run {}: {}",
                blocker.run,
                blocker_kind_name(blocker.kind)
            ));
        }
    }
    if let Some(reason) = s.cancel_reason {
        lines.push(format!(
            "  last restart cancelled: {}",
            cancel_reason(reason)
        ));
    }
    let audit = &status.audit;
    if audit.denial_persistence_failures.get() > 0 || audit.denial_loss.is_some() {
        lines.push(format!(
            "  audit trouble: {} denial record(s) could not be stored",
            audit.denial_persistence_failures.get()
        ));
        if let Some(loss) = &audit.denial_loss {
            lines.push(format!(
                "  audit trouble: {} denial(s) lost between {} and {} (ms)",
                loss.count.get(),
                loss.first_ms.get(),
                loss.last_ms.get()
            ));
        }
    }
    lines
}

pub fn describe_blockers(blockers: &ShellBlockers) -> Vec<String> {
    let mut lines = Vec::new();
    for (i, b) in blockers.blockers.iter().enumerate() {
        lines.push(format!(
            "  [{i}] run {} (blocker {}, phase {}, teardown {}, result {})",
            b.run,
            b.binding.blocker_identity.as_str(),
            phase_name(b.binding.phase),
            if b.teardown_confirmed {
                "confirmed"
            } else {
                "not confirmed"
            },
            if b.observed {
                "observed"
            } else {
                "not observed"
            }
        ));
    }
    if blockers.omitted.get() > 0 {
        lines.push(format!("  {} more not listed", blockers.omitted.get()));
    }
    lines
}

pub fn describe_receipt(receipt: &AdminReceipt) -> Vec<String> {
    let state = match receipt.state {
        AdminOperationState::Pending => "accepted, not started",
        AdminOperationState::Started => "started",
        AdminOperationState::Completed => "completed",
        AdminOperationState::Unknown => "state unknown",
    };
    let outcome = match receipt.outcome {
        AdminOutcome::Pending => "pending",
        AdminOutcome::Succeeded => "succeeded",
        AdminOutcome::Failed => "failed",
        AdminOutcome::Cancelled => "cancelled",
        AdminOutcome::Conflict => "conflict (the target changed)",
        AdminOutcome::Unknown => "unknown",
    };
    let mut lines = vec![format!(
        "operation {}: {state}, outcome {outcome}",
        receipt.operation_id.as_str()
    )];
    if receipt.audit_warning {
        lines.push(
            "warning: the audit record for this operation could not be stored in full".into(),
        );
    }
    lines
}

/// Where the reconnect stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReconnectView {
    pub stage: Stage,
    pub remaining: Duration,
}

pub fn describe_reconnect(view: &ReconnectView) -> String {
    let (what, total) = match view.stage {
        Stage::RestartingToReady => (
            "restarting: connecting only, no new core is started; waiting for the successor",
            120,
        ),
        Stage::ReadyToReconnect => ("the successor is ready; rebuilding this session", 30),
    };
    format!("{what} ({}s left of {total}s)", view.remaining.as_secs())
}

pub fn describe_expiry(stage: Stage) -> String {
    match stage {
        Stage::RestartingToReady => "the successor core did not accept connections within 120 seconds; the core was not killed and no other core was started".into(),
        Stage::ReadyToReconnect => "the successor was ready but this session was not rebuilt within 30 seconds; nothing was resent".into(),
    }
}
