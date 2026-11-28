//! Owned-graph contracts only. Views and opaque references never grant authority.
use super::contracts::*;
use crate::{validation::ValidationError, wire::Revision};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThreadCas {
    pub thread_ref: AdminId,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OwnedGraphBinding {
    pub graph: GraphCas,
    /// Complete canonical thread inventory, not a subset.
    pub threads: Vec<ThreadCas>,
    /// Zero expects no control row.
    pub expected_control_revision: Revision,
}
impl OwnedGraphBinding {
    pub fn validate(&self) -> Result<(), ValidationError> {
        items(&self.threads)?;
        if self
            .threads
            .windows(2)
            .any(|p| p[0].thread_ref >= p[1].thread_ref)
        {
            return Err(inconsistent(
                "owned_graphs.threads",
                "thread references must be strictly increasing",
            ));
        }
        bounded(self)
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfirmedOwnedGraph {
    pub binding: OwnedGraphBinding,
    pub confirmed: bool,
}
impl ConfirmedOwnedGraph {
    pub fn validate(&self) -> Result<(), ValidationError> {
        self.binding.validate()?;
        if !self.confirmed {
            return Err(inconsistent(
                "owned_graphs.confirmed",
                "explicit confirmation required",
            ));
        }
        bounded(self)
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum OwnedGraphsCommand {
    Release(ConfirmedOwnedGraph),
    Resume(ConfirmedOwnedGraph),
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum OwnedGraphsQuery {
    Undisposed,
    Detail { graph_ref: AdminId },
    Operation { operation_id: AdminId },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GraphReference {
    pub graph_id: AdminId,
    pub restart_tx_id: AdminId,
    pub capture_id: AdminId,
    pub revision: Revision,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IncompleteDisposals {
    pub count: Revision,
    pub references: Vec<GraphReference>,
    pub truncated: bool,
}
impl IncompleteDisposals {
    pub fn empty() -> Self {
        Self {
            count: Revision(0),
            references: vec![],
            truncated: false,
        }
    }
    pub fn validate(&self) -> Result<(), ValidationError> {
        collection(self.count, &self.references, self.truncated)?;
        bounded(self)
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UndisposedGraphs {
    /// Includes otherwise eligible graphs whose thread inventory exceeds 64.
    pub count: Revision,
    pub references: Vec<GraphReference>,
    pub snapshot_revision: Revision,
    pub truncated: bool,
    pub over_limit_count: Revision,
    /// Prior confirmed revocation with cleanup left after generation changed;
    /// these are warnings, not current restart blockers.
    pub incomplete_disposals: IncompleteDisposals,
}
impl UndisposedGraphs {
    pub fn validate(&self) -> Result<(), ValidationError> {
        collection(self.count, &self.references, self.truncated)?;
        if self.over_limit_count > self.count {
            return Err(inconsistent(
                "owned_graphs.over_limit_count",
                "cannot exceed total graph count",
            ));
        }
        self.incomplete_disposals.validate()?;
        bounded(self)
    }
}
fn collection<T>(count: Revision, refs: &[T], truncated: bool) -> Result<(), ValidationError> {
    items(refs)?;
    if count.get() < refs.len() as u64 || truncated != (count.get() > refs.len() as u64) {
        return Err(inconsistent(
            "owned_graphs.count",
            "count must match references and truncation",
        ));
    }
    Ok(())
}
fn inconsistent(field: &'static str, reason: &'static str) -> ValidationError {
    ValidationError::Inconsistent { field, reason }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RestartPhaseView {
    Preparing,
    Capturing,
    Committed,
    Armed,
    Adopted,
    Completed,
    Aborted,
    Invalidated,
    Superseded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RestartDispositionView {
    RetryPending,
    Activated,
    Released,
    Quarantined,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaimView {
    Absent,
    Prepared,
    Activated,
    Released,
    Unreadable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GraphEligibilityView {
    PreAdoption,
    AdoptedRetryPending,
    CompletedCleanupOnly,
    TerminalCleanupOnly,
    Activated,
    Executing,
    Revoked,
    ColdIsolated,
    NeedsAttention,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GraphControlView {
    Open,
    AutomaticReserved,
    ManualPending,
    ManualIdle,
    ManualRunning,
    RevokedIdle,
    RevokedRunning,
    Resolved,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GraphAuthorityProblem {
    CurrentBindingRequired,
    CurrentSettingsRequired,
    CliContinuityRequired,
    TeamReconciliationRequired,
    StorageUnavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GraphProblemDetail {
    NoClaim,
    PreAdoption,
    Completed,
    TerminalTransaction,
    AlreadyActivated,
    Released,
    NeedsAttention,
    CatalogUnavailable,
    Cold,
    ThreadInUse,
    UseUnknown,
    ConcurrentOperation,
    EligibilityRevoked,
    ResultUnknown,
    TooManyThreads,
    CurrentBindingRequired,
    CurrentSettingsRequired,
    CliContinuityRequired,
    TeamReconciliationRequired,
    StorageUnavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepObservation {
    NotStarted,
    ConfirmedNotApplied,
    ConfirmedApplied,
    Unknown,
    NotApplicable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DisposalCompletion {
    NotStarted,
    InProgress,
    Complete,
    Partial,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectStep {
    Eligibility,
    PreparedRelease,
    Scratch,
    Activation,
    AuditFinalize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NextAction {
    ObserveSameOperation,
    RefreshAndConfirmRelease,
    RefreshAndConfirmResume,
    WaitAutomaticRetryOrRelease,
    ReleaseOnly,
    NormalStopRequired,
    None,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum OperationLimitView {
    WithinLimit,
    TooManyThreads { limit: u32 },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ActionAvailability {
    Available,
    Blocked {
        problem: GraphAuthorityProblem,
        detail: GraphProblemDetail,
    },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DisposalProgressView {
    pub eligibility: StepObservation,
    pub prepared_release: StepObservation,
    pub scratch: StepObservation,
    pub completion: DisposalCompletion,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OwnedGraphView {
    pub graph_ref: AdminId,
    pub snapshot_revision: Revision,
    /// Server-observed expectations only. Detail uses the valid placeholder
    /// operation ID "unassigned". Clients replace it with a fresh operation ID
    /// on confirmation; retransmissions keep that client-assigned ID unchanged.
    /// The binding is diagnostic data, never execution authority.
    pub binding: OwnedGraphBinding,
    pub restart_phase: RestartPhaseView,
    pub disposition: RestartDispositionView,
    pub claim: ClaimView,
    pub control: GraphControlView,
    pub eligibility: GraphEligibilityView,
    pub resume: ActionAvailability,
    pub release: ActionAvailability,
    pub progress: DisposalProgressView,
    pub thread_count: Revision,
    pub thread_refs: Vec<ThreadCas>,
    pub thread_refs_truncated: bool,
    pub operation_limit: OperationLimitView,
}
impl OwnedGraphView {
    pub fn validate(&self) -> Result<(), ValidationError> {
        self.binding.validate()?;
        collection(
            self.thread_count,
            &self.thread_refs,
            self.thread_refs_truncated,
        )?;
        if self.graph_ref != self.binding.graph.graph_id || self.thread_refs != self.binding.threads
        {
            return Err(inconsistent(
                "owned_graphs.detail",
                "view and expected binding must identify the same graph and threads",
            ));
        }
        match self.operation_limit {
            OperationLimitView::WithinLimit
                if self.thread_count.get() <= MAX_ADMIN_ITEMS as u64 => {}
            OperationLimitView::TooManyThreads { limit }
                if limit == MAX_ADMIN_ITEMS as u32
                    && self.thread_count.get() > limit as u64
                    && !matches!(self.release, ActionAvailability::Available)
                    && !matches!(self.resume, ActionAvailability::Available) => {}
            _ => {
                return Err(inconsistent(
                    "owned_graphs.operation_limit",
                    "over-limit graphs are display-only",
                ));
            }
        }
        bounded(self)
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GraphOperationView {
    /// A historical Failed result does not prove that no effect was applied.
    pub receipt: AdminReceipt,
    pub graph_ref: AdminId,
    pub step: EffectStep,
    pub observation: StepObservation,
    pub progress: DisposalProgressView,
    pub next_action: NextAction,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum OwnedGraphsReply {
    Undisposed(UndisposedGraphs),
    Detail(Box<OwnedGraphView>),
    Operation(GraphOperationView),
}
impl OwnedGraphsReply {
    pub fn validate(&self) -> Result<(), ValidationError> {
        match self {
            Self::Undisposed(g) => g.validate(),
            Self::Detail(g) => g.validate(),
            Self::Operation(g) => bounded(g),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::InstanceEpoch;
    fn id(s: &str) -> AdminId {
        AdminId::new(s).unwrap()
    }
    fn binding(n: usize) -> OwnedGraphBinding {
        OwnedGraphBinding {
            graph: GraphCas {
                graph_id: id("g"),
                restart_tx_id: id("tx"),
                capture_id: id("cap"),
                expected_revision: Revision(1),
                expected_core_epoch: InstanceEpoch(2),
                operation_id: id("unassigned"),
            },
            threads: (0..n)
                .map(|n| ThreadCas {
                    thread_ref: id(&format!("thread-{n:03}")),
                })
                .collect(),
            expected_control_revision: Revision(0),
        }
    }
    #[test]
    fn thread_inventory_is_complete_sorted_and_bounded() {
        assert!(binding(0).validate().is_ok());
        assert!(binding(64).validate().is_ok());
        assert!(binding(65).validate().is_err());
        let mut b = binding(2);
        b.threads.swap(0, 1);
        assert!(b.validate().is_err());
        let mut b = binding(2);
        b.threads[1] = b.threads[0].clone();
        assert!(b.validate().is_err());
        let mut b = binding(1);
        b.threads.clear();
        assert!(b.validate().is_ok());
    }
    fn progress() -> DisposalProgressView {
        DisposalProgressView {
            eligibility: StepObservation::ConfirmedApplied,
            prepared_release: StepObservation::Unknown,
            scratch: StepObservation::NotStarted,
            completion: DisposalCompletion::Unknown,
        }
    }
    fn view() -> OwnedGraphView {
        let binding = binding(1);
        OwnedGraphView {
            graph_ref: id("g"),
            snapshot_revision: Revision(1),
            thread_refs: binding.threads.clone(),
            binding,
            restart_phase: RestartPhaseView::Adopted,
            disposition: RestartDispositionView::RetryPending,
            claim: ClaimView::Absent,
            control: GraphControlView::ManualPending,
            eligibility: GraphEligibilityView::AdoptedRetryPending,
            resume: ActionAvailability::Blocked {
                problem: GraphAuthorityProblem::CurrentBindingRequired,
                detail: GraphProblemDetail::NoClaim,
            },
            release: ActionAvailability::Available,
            progress: progress(),
            thread_count: Revision(1),
            thread_refs_truncated: false,
            operation_limit: OperationLimitView::WithinLimit,
        }
    }
    #[test]
    fn detail_binding_can_be_used_to_confirm_release_and_unknown_is_not_no_effect() {
        let v = view();
        v.validate().unwrap();
        let release = ConfirmedOwnedGraph {
            binding: v.binding.clone(),
            confirmed: true,
        };
        release.validate().unwrap();
        assert_eq!(release.binding.graph.operation_id, id("unassigned"));
        let op = OwnedGraphsReply::Operation(GraphOperationView {
            receipt: AdminReceipt {
                operation_id: id("client-op"),
                state: AdminOperationState::Completed,
                outcome: AdminOutcome::Failed,
                audit_warning: false,
            },
            graph_ref: id("g"),
            step: EffectStep::PreparedRelease,
            observation: StepObservation::Unknown,
            progress: progress(),
            next_action: NextAction::ObserveSameOperation,
        });
        op.validate().unwrap();
        let json = serde_json::to_value(op).unwrap();
        assert_eq!(json["payload"]["progress"].as_object().unwrap().len(), 4);
        assert_eq!(
            json["payload"]["progress"]["eligibility"],
            "confirmed_applied"
        );
        assert_eq!(json["payload"]["observation"], "unknown");
        assert_ne!(
            serde_json::to_value(NextAction::WaitAutomaticRetryOrRelease).unwrap(),
            serde_json::to_value(NextAction::ReleaseOnly).unwrap()
        );
    }
    #[test]
    fn view_limits_and_summary_counts_are_consistent() {
        let mut v = view();
        v.thread_count = Revision(65);
        v.thread_refs_truncated = true;
        assert!(v.validate().is_err());
        v.operation_limit = OperationLimitView::TooManyThreads { limit: 64 };
        assert!(v.validate().is_err()); // Over-limit diagnostic must not offer effects.
        v.release = v.resume.clone();
        v.validate().unwrap();
        v.operation_limit = OperationLimitView::TooManyThreads { limit: 65 };
        assert!(v.validate().is_err());
        let mut g = UndisposedGraphs {
            count: Revision(1),
            references: vec![],
            snapshot_revision: Revision(1),
            truncated: true,
            over_limit_count: Revision(1),
            incomplete_disposals: IncompleteDisposals::empty(),
        };
        g.validate().unwrap();
        g.over_limit_count = Revision(2);
        assert!(g.validate().is_err());
        g.over_limit_count = Revision(0);
        g.incomplete_disposals.count = Revision(1);
        assert!(g.validate().is_err());
        g.incomplete_disposals.truncated = true;
        g.validate().unwrap();
        let r = GraphReference {
            graph_id: id("g"),
            restart_tx_id: id("tx"),
            capture_id: id("cap"),
            revision: Revision(1),
        };
        g.references = vec![r; 65];
        g.count = Revision(65);
        g.truncated = false;
        assert!(g.validate().is_err());
    }
}
