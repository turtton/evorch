use std::time::Duration;
use workbench_client::{
    ClientError, TransportError,
    restart::{reconnect::Stage, view::*},
};
use workbench_protocol::{
    ErrorCode, RetryAdvice, WorkbenchError,
    ids::RequestId,
    namespaces::lifecycle::{AdminId, AdminOperationState, AdminOutcome, AdminReceipt},
};

fn server(code: ErrorCode) -> ClientError {
    ClientError::Server(WorkbenchError::new(code, "why", RetryAdvice::Never))
}

#[test]
fn a_changed_target_is_shown_as_a_conflict_to_read_again_not_as_a_failure() {
    for code in [
        ErrorCode::Conflict,
        ErrorCode::StaleRevision,
        ErrorCode::StaleEpoch,
    ] {
        assert_eq!(classify(&server(code)), FailureKind::Stale);
    }
    let text = describe_failure(&server(ErrorCode::Conflict));
    assert!(text.contains("conflict"), "{text}");
    assert!(text.contains("nothing was changed"), "{text}");
}

#[test]
fn failures_are_told_apart() {
    assert_eq!(classify(&server(ErrorCode::Busy)), FailureKind::Busy);
    assert_eq!(
        classify(&server(ErrorCode::UnsupportedCapability)),
        FailureKind::Unsupported
    );
    assert_eq!(
        classify(&server(ErrorCode::PermissionDenied)),
        FailureKind::Denied
    );
    let lost = ClientError::Transport(TransportError::OutcomeUnknown {
        request_id: RequestId::new("c.1").unwrap(),
    });
    assert_eq!(classify(&lost), FailureKind::Unknown);
    assert!(describe_failure(&lost).contains("not sent again"));
    assert_eq!(
        classify(&ClientError::Transport(TransportError::Disconnected)),
        FailureKind::Other
    );
}

#[test]
fn an_audit_failure_is_a_warning_on_the_receipt() {
    let mut receipt = AdminReceipt {
        operation_id: AdminId::new("op-1").unwrap(),
        state: AdminOperationState::Completed,
        outcome: AdminOutcome::Succeeded,
        audit_warning: false,
    };
    assert_eq!(describe_receipt(&receipt).len(), 1);
    receipt.audit_warning = true;
    let lines = describe_receipt(&receipt);
    assert_eq!(lines.len(), 2);
    assert!(lines[1].contains("audit"), "{lines:?}");
    receipt.outcome = AdminOutcome::Conflict;
    assert!(describe_receipt(&receipt)[0].contains("conflict"));
}

#[test]
fn the_reconnect_line_names_the_stage_and_the_time_left() {
    let waiting = describe_reconnect(&ReconnectView {
        stage: Stage::RestartingToReady,
        remaining: Duration::from_secs(97),
    });
    assert!(waiting.contains("97s left of 120s"), "{waiting}");
    assert!(waiting.contains("no new core is started"), "{waiting}");
    let ready = describe_reconnect(&ReconnectView {
        stage: Stage::ReadyToReconnect,
        remaining: Duration::from_secs(12),
    });
    assert!(ready.contains("12s left of 30s"), "{ready}");
    assert!(describe_expiry(Stage::RestartingToReady).contains("120 seconds"));
    assert!(describe_expiry(Stage::ReadyToReconnect).contains("30 seconds"));
}
