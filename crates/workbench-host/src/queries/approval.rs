//! Approval input is re-authorized against the canonical registry on every read.
use sandbox::approval::ApprovalError;
use workbench_protocol::{
    errors::{ErrorCode, WorkbenchError},
    queries::{ApprovalInputChunk, GetApprovalInput},
};

pub(crate) fn registry_error(error: ApprovalError) -> WorkbenchError {
    let (code, message) = match error {
        ApprovalError::WrongEpoch => (
            ErrorCode::StaleEpoch,
            "approval belongs to another core epoch",
        ),
        ApprovalError::RevisionMismatch { .. } => {
            (ErrorCode::StaleRevision, "approval revision changed")
        }
        ApprovalError::AlreadyDecided(_) | ApprovalError::Expired | ApprovalError::Cancelled => {
            (ErrorCode::AlreadyResolved, "approval is no longer pending")
        }
        ApprovalError::InputTooLarge { .. } => {
            (ErrorCode::LimitExceeded, "approval input exceeds 1 MiB")
        }
        ApprovalError::Unknown => (ErrorCode::NotFound, "unknown approval"),
    };
    super::error(code, message)
}

pub(crate) fn chunk(
    request: &GetApprovalInput,
    input: Option<std::sync::Arc<serde_json::Value>>,
) -> Result<ApprovalInputChunk, WorkbenchError> {
    // Preserve the registry's empty-input convention. Redact the entire value
    // before slicing so secrets spanning chunk boundaries cannot leak.
    let input = input.map(|v| v.to_string()).unwrap_or_default();
    let masked = super::masked(&input);
    Ok(ApprovalInputChunk::slice(request, &masked)?)
}
