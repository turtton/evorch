//! Host service of the `evaluation` area (PR 13 reads, PR 17c runs). Placeholder: the namespace is reserved,
//! so nothing reaches this service and it starts no task, opens no file and
//! talks to nothing. The PR that implements the area fills in this file, the
//! adapter `ingress/namespaces/evaluation.rs` and the protocol module, and nothing
//! shared.
//!
//! Takes from the handles: `database` and `config_dir` for the report and replay readers, `evaluations` and `evaluator` for the jobs (no evaluation machinery of its own).
use super::AreaHandles;
use crate::bootstrap::BootstrapError;

/// The `evaluation` service, reached from `ingress/namespaces/evaluation.rs` as
/// `self.areas.evaluation`.
#[derive(Default)]
pub struct EvaluationArea {}

impl EvaluationArea {
    pub(super) fn new(_handles: &AreaHandles) -> Result<Self, BootstrapError> {
        Ok(Self::default())
    }
}
