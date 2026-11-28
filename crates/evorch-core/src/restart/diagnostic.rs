//! Process-local terminal evidence, retained after reopening admission. The
//! existing protocol has no diagnostic DTO; this ledger is for core readback.
use super::*;
use runtime::quiesce::RestartBlockerKind;
use workbench_protocol::namespaces::lifecycle::AdminId;

const MAX_DIAGNOSTICS: usize = 32;
const MAX_CAUSES: usize = 16;
const MAX_THREAD_BYTES: usize = 128;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PreparationStage {
    EvaluationDrain,
    LearningDrain,
    RecipeObservation,
    ProducerDrain,
    SafePoints,
    HostDrain,
    DispatchTailDrain,
    StorageSeal,
    TargetValidation,
    AuditMapping,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PreparationFailure {
    Deadline,
    ObservationExhausted,
    RecipeUnavailable,
    MemberChanged,
    BindingChanged,
    ApprovalPending,
    DrainFailed,
    Storage,
    TargetChanged,
    AuditUnavailable,
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreparationCause {
    pub run: RunId,
    pub thread: Option<String>,
    pub blocker: Option<RestartBlockerKind>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreparationDiagnostic {
    pub operation: Option<AdminId>,
    pub attempt: RestartId,
    pub stage: PreparationStage,
    pub reason: PreparationFailure,
    pub causes: Vec<PreparationCause>,
    pub total: usize,
    pub omitted: usize,
    /// Whether this attempt left admission open, not a promise that a later
    /// request will pass its independent preflight checks.
    pub retryable: bool,
}

impl RestartCoordinator {
    pub fn preparation_diagnostic(&self, attempt: RestartId) -> Option<PreparationDiagnostic> {
        self.0
            .state
            .lock()
            .ok()?
            .diagnostics
            .iter()
            .find(|d| d.attempt == attempt)
            .cloned()
    }

    pub fn operation_diagnostic(&self, operation: &AdminId) -> Option<PreparationDiagnostic> {
        self.0
            .state
            .lock()
            .ok()?
            .diagnostics
            .iter()
            .rev()
            .find(|d| d.operation.as_ref() == Some(operation))
            .cloned()
    }

    pub(super) fn preparation_stage(&self, id: RestartId, stage: PreparationStage) {
        let mut state = self.0.state.lock().unwrap();
        if state.id == Some(id) && !state.capture_started {
            state.preparation_stage = stage;
        }
    }

    pub(super) fn record_preparation_failure(
        &self,
        state: &mut State,
        reason: PreparationFailure,
        run: Option<RunId>,
    ) {
        let Some(attempt) = state.id else {
            return;
        };
        // The first reason wins even when cancellation finishes asynchronously.
        if state
            .diagnostics
            .back()
            .is_some_and(|d| d.attempt == attempt)
        {
            return;
        }
        let pending = (reason == PreparationFailure::ApprovalPending)
            .then(|| self.0.runtime.pending_approvals());
        let blockers = self.0.runtime.restart_blockers();
        let cause = |run, blocker| PreparationCause {
            run,
            // Only consult the in-memory thread directory here.
            thread: self
                .0
                .runtime
                .thread_of_live_run(run)
                .filter(|t| t.len() <= MAX_THREAD_BYTES),
            blocker,
        };
        let (total, causes) = if let Some(approvals) = &pending {
            let runs: std::collections::BTreeSet<RunId> = approvals
                .iter()
                .filter_map(|approval| approval.run_id.parse().ok())
                .collect();
            (
                runs.len(),
                runs.into_iter()
                    .take(MAX_CAUSES)
                    .map(|run| cause(run, Some(RestartBlockerKind::ApprovalPending)))
                    .collect(),
            )
        } else if let Some(run) = run {
            (1, vec![cause(run, None)])
        } else {
            (
                blockers.len(),
                blockers
                    .iter()
                    .take(MAX_CAUSES)
                    .map(|b| cause(b.run, Some(b.kind)))
                    .collect(),
            )
        };
        let diagnostic = PreparationDiagnostic {
            operation: state.operation.clone(),
            attempt,
            stage: state.preparation_stage,
            reason,
            omitted: total - causes.len(),
            causes,
            total,
            retryable: false,
        };
        if state.diagnostics.len() == MAX_DIAGNOSTICS {
            state.diagnostics.pop_front();
        }
        tracing::warn!(?diagnostic, "restart preparation terminated");
        state.diagnostics.push_back(diagnostic);
    }

    pub(super) fn cancel_for_failure(
        &self,
        id: RestartId,
        reason: PreparationFailure,
        run: Option<RunId>,
    ) {
        let _ = self.cancel_with_diagnostic(id, None, None, reason, run);
    }
}
