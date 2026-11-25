//! The desktop's wording. What is distinguished comes from
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
        RestartPhase::Running => "Running",
        RestartPhase::Requested => "Requested",
        RestartPhase::PreparingDrains => "Preparing",
        RestartPhase::WaitingSafePoint => "Waiting for a safe point",
        RestartPhase::Sealing => "Sealing",
        RestartPhase::Sealed => "Sealed",
        RestartPhase::Capturing => "Capturing state",
        RestartPhase::Finalizing => "Finalizing",
        RestartPhase::Cancelling => "Cancelling",
        RestartPhase::FailedClosed => "Failed closed",
        RestartPhase::ShuttingDown => "Shutting down",
    }
}

pub fn failure(error: &ClientError) -> String {
    match classify(error) {
        FailureKind::Stale => {
            "Conflict: the target or state you confirmed is no longer current. Nothing was changed. Read it again and decide again.".into()
        }
        FailureKind::Busy => "The core is busy. Nothing was changed.".into(),
        FailureKind::ActionRequired => match error {
            ClientError::Server(e) => format!(
                "Not accepted: something needs to be handled first ({}). Nothing was changed.",
                e.message
            ),
            other => other.to_string(),
        },
        FailureKind::Unsupported => {
            "This core does not offer safe restart. Nothing was changed.".into()
        }
        FailureKind::Denied => "This operation is not permitted. Nothing was changed.".into(),
        FailureKind::Unknown => {
            "The outcome is unknown. The request was not sent again; read the operation again to check it.".into()
        }
        FailureKind::Other => match error {
            ClientError::Server(e) => e.message.clone(),
            other => other.to_string(),
        },
    }
}

pub fn approval_blocker(run: &RunId) -> String {
    format!("Run {run} is waiting for approval. Approve or deny it, or stop the run.")
}

pub fn cancel_reason(reason: RestartCancelReason) -> String {
    match reason {
        RestartCancelReason::ApprovalPending { pending } => format!(
            "The restart was cancelled because {pending} approval(s) were pending. Approve or deny them, or stop the run, then request it again."
        ),
    }
}

pub fn receipt(receipt: &AdminReceipt) -> Vec<String> {
    let state = match receipt.state {
        AdminOperationState::Pending => "Accepted (not started)",
        AdminOperationState::Started => "Started",
        AdminOperationState::Completed => "Completed",
        AdminOperationState::Unknown => "State unknown",
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
        "Operation {}: {state}, outcome {outcome}",
        receipt.operation_id.as_str()
    )];
    if receipt.audit_warning {
        lines
            .push("Warning: the audit record of this operation could not be stored in full".into());
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
        "Cannot restart: {count} graph(s) from the previous restart are not disposed of. Run `evorch core graphs list` (quarantined ones: `evorch core quarantine list`) with the admin CLI, release, resume or discard them, then try again. Nothing was changed."
    )
}

pub fn outcome_unknown() -> String {
    "The outcome is unknown. The request was not sent again; the operation's state is read again after reconnecting.".into()
}
