use crate::admin::{
    self, AdminFailure, GraphDisposalFacts, GraphReferences, RevocationObservation,
};
use std::collections::{BTreeMap, BTreeSet};
use storage::recovery::{
    RecoveryClaimState, RecoveryGraphHeader,
    graph_control::GraphControlCas,
    graph_progress::GraphDisposalStep,
    owned_graphs::OwnedGraphSnapshot,
    restart::{RestartDisposition, RestartPhase},
};
use workbench_protocol::{
    errors::ErrorCode,
    namespaces::lifecycle::{AdminId, GraphCas, owned_graphs::*},
    wire::{InstanceEpoch, Revision},
};
#[cfg(test)]
#[path = "view_tests.rs"]
mod tests;

fn unavailable() -> AdminFailure {
    admin::failure(
        ErrorCode::StorageUnavailable,
        "owned graph observation unavailable",
    )
}

/// The threads of a graph's captured members, in canonical order.
pub(super) fn thread_ids(cas: &GraphControlCas) -> BTreeSet<String> {
    cas.membership
        .iter()
        .filter_map(|m| m.thread_id.clone())
        .collect()
}

/// Complete raw expectations remain server-side. The returned value grants no
/// execution or disposal authority; the operation resolves its own typed
/// authority from current bindings.
pub fn binding_for(
    cas: &GraphControlCas,
    operation_id: AdminId,
) -> Result<OwnedGraphBinding, AdminFailure> {
    let mut refs = GraphReferences::default();
    let manifest = &cas.manifest.manifest_id;
    let graph_id = refs.graph_reference(&cas.restart_id, &cas.capture_id, manifest)?;
    let restart_tx_id = refs.graph_reference("transaction", &cas.restart_id, &cas.restart_id)?;
    let capture_id = refs.graph_reference("capture", &cas.restart_id, &cas.capture_id)?;
    let mut threads = Vec::new();
    for thread in thread_ids(cas) {
        threads.push(ThreadCas {
            thread_ref: refs.thread_reference(
                &cas.restart_id,
                &cas.capture_id,
                manifest,
                &thread,
            )?,
        });
    }
    threads.sort_by(|a, b| a.thread_ref.cmp(&b.thread_ref));
    threads.truncate(64);
    let binding = OwnedGraphBinding {
        graph: GraphCas {
            graph_id,
            restart_tx_id,
            capture_id,
            expected_revision: Revision(cas.component.revision),
            expected_core_epoch: InstanceEpoch(cas.successor_epoch),
            operation_id,
        },
        threads,
        expected_control_revision: Revision(cas.control_revision),
    };
    binding.validate().map_err(|_| unavailable())?;
    Ok(binding)
}

pub(super) fn progress(graph: &OwnedGraphSnapshot) -> DisposalProgressView {
    let revoked = graph.control.as_ref().is_some_and(|c| c.revoked);
    let threads = thread_ids(&graph.cas);
    let n = threads.len();
    // One row per thread: the key no longer carries a generation.
    let latest = graph
        .progress
        .iter()
        .filter(|p| threads.contains(&p.key.thread_id))
        .map(|p| (&p.key.thread_id, p))
        .collect::<BTreeMap<_, _>>();
    let steps = latest.values().map(|p| p.scratch).collect::<Vec<_>>();
    let scratch = if steps.contains(&GraphDisposalStep::Unknown) {
        StepObservation::Unknown
    } else if !steps.is_empty()
        && steps.len() == n
        && steps.iter().all(|s| {
            matches!(
                s,
                GraphDisposalStep::Applied | GraphDisposalStep::NotApplicable
            )
        })
    {
        if steps.iter().all(|s| *s == GraphDisposalStep::NotApplicable) {
            StepObservation::NotApplicable
        } else {
            StepObservation::ConfirmedApplied
        }
    } else {
        StepObservation::NotStarted
    };
    DisposalProgressView {
        eligibility: if revoked {
            StepObservation::ConfirmedApplied
        } else {
            StepObservation::NotStarted
        },
        prepared_release: match graph.cas.claim.as_ref().map(|c| &c.state) {
            None => StepObservation::NotApplicable,
            Some(RecoveryClaimState::Released { .. }) if revoked => {
                StepObservation::ConfirmedApplied
            }
            _ => StepObservation::NotStarted,
        },
        scratch,
        completion: if !revoked {
            DisposalCompletion::NotStarted
        } else if matches!(
            scratch,
            StepObservation::ConfirmedApplied | StepObservation::NotApplicable
        ) {
            DisposalCompletion::Complete
        } else if scratch == StepObservation::Unknown {
            DisposalCompletion::Unknown
        } else {
            DisposalCompletion::Partial
        },
    }
}
fn phase(p: RestartPhase) -> RestartPhaseView {
    match p {
        RestartPhase::Preparing => RestartPhaseView::Preparing,
        RestartPhase::Capturing => RestartPhaseView::Capturing,
        RestartPhase::Committed => RestartPhaseView::Committed,
        RestartPhase::Armed => RestartPhaseView::Armed,
        RestartPhase::Adopted => RestartPhaseView::Adopted,
        RestartPhase::Completed => RestartPhaseView::Completed,
        RestartPhase::Aborted => RestartPhaseView::Aborted,
        RestartPhase::Invalidated => RestartPhaseView::Invalidated,
        RestartPhase::Superseded => RestartPhaseView::Superseded,
    }
}
fn disposition(d: RestartDisposition) -> RestartDispositionView {
    match d {
        RestartDisposition::RetryPending => RestartDispositionView::RetryPending,
        RestartDisposition::Activated => RestartDispositionView::Activated,
        RestartDisposition::Released => RestartDispositionView::Released,
        RestartDisposition::Quarantined => RestartDispositionView::Quarantined,
    }
}
fn blocked(detail: GraphProblemDetail) -> ActionAvailability {
    ActionAvailability::Blocked {
        problem: GraphAuthorityProblem::CurrentBindingRequired,
        detail,
    }
}
/// Project independent persisted facts, not a second lifecycle state machine.
/// Execution authority is separately checked again inside the core port, against
/// the thread and root bindings current at that moment.
///
/// `live_epoch` is the epoch of this core. A graph is acted on only when it was
/// adopted by that very epoch: one adopted by an earlier core (and left by its
/// crash) is cold, whatever its row says.
pub fn detail(
    graph: &OwnedGraphSnapshot,
    live_epoch: Option<InstanceEpoch>,
) -> Result<OwnedGraphView, AdminFailure> {
    let binding = binding_for(
        &graph.cas,
        AdminId::new("unassigned").map_err(|_| unavailable())?,
    )?;
    let thread_count = thread_ids(&graph.cas).len();
    let cold = !adopted_by(graph, live_epoch);
    let claim = match graph.cas.claim.as_ref().map(|c| &c.state) {
        None => ClaimView::Absent,
        Some(RecoveryClaimState::Prepared) => ClaimView::Prepared,
        Some(RecoveryClaimState::Activated) => ClaimView::Activated,
        Some(RecoveryClaimState::Released { .. }) => ClaimView::Released,
    };
    let revoked = graph.control.as_ref().is_some_and(|c| c.revoked);
    let control = match graph.control.as_ref().map(|c| c.mode.as_str()) {
        None => GraphControlView::Open,
        Some("manual_running") => GraphControlView::ManualRunning,
        Some("manual_idle") => GraphControlView::ManualIdle,
        Some("revoked_running") => GraphControlView::RevokedRunning,
        Some("revoked_idle") => GraphControlView::RevokedIdle,
        Some("resolved") => GraphControlView::Resolved,
        _ => GraphControlView::Unknown,
    };
    let terminal = matches!(
        graph.cas.phase,
        RestartPhase::Aborted | RestartPhase::Invalidated | RestartPhase::Superseded
    );
    let preadoption = !matches!(
        graph.cas.phase,
        RestartPhase::Adopted
            | RestartPhase::Completed
            | RestartPhase::Aborted
            | RestartPhase::Invalidated
            | RestartPhase::Superseded
    );
    let activated = claim == ClaimView::Activated
        || graph.cas.component.disposition == RestartDisposition::Activated;
    let eligibility = if preadoption {
        GraphEligibilityView::PreAdoption
    } else if activated {
        GraphEligibilityView::Activated
    } else if revoked {
        GraphEligibilityView::Revoked
    } else if terminal {
        GraphEligibilityView::TerminalCleanupOnly
    } else if graph.cas.phase == RestartPhase::Completed {
        GraphEligibilityView::CompletedCleanupOnly
    } else if cold {
        GraphEligibilityView::ColdIsolated
    } else if graph.cas.component.disposition == RestartDisposition::RetryPending {
        GraphEligibilityView::AdoptedRetryPending
    } else {
        GraphEligibilityView::NeedsAttention
    };
    let release = if thread_count > 64 {
        blocked(GraphProblemDetail::TooManyThreads)
    } else if preadoption {
        blocked(GraphProblemDetail::PreAdoption)
    } else if activated {
        blocked(GraphProblemDetail::AlreadyActivated)
    } else if (terminal || graph.cas.phase == RestartPhase::Completed)
        && claim == ClaimView::Prepared
    {
        blocked(GraphProblemDetail::NeedsAttention)
    } else if cold {
        blocked(GraphProblemDetail::Cold)
    } else if matches!(
        control,
        GraphControlView::ManualRunning | GraphControlView::RevokedRunning
    ) {
        blocked(GraphProblemDetail::ConcurrentOperation)
    } else {
        ActionAvailability::Available
    };
    let resume = if !matches!(release, ActionAvailability::Available) {
        release.clone()
    } else if revoked {
        blocked(GraphProblemDetail::EligibilityRevoked)
    } else if graph.cas.phase == RestartPhase::Completed {
        blocked(GraphProblemDetail::Completed)
    } else if terminal {
        blocked(GraphProblemDetail::TerminalTransaction)
    } else if claim != ClaimView::Prepared {
        blocked(if claim == ClaimView::Absent {
            GraphProblemDetail::NoClaim
        } else {
            GraphProblemDetail::Released
        })
    } else {
        ActionAvailability::Available
    };
    let result = OwnedGraphView {
        graph_ref: binding.graph.graph_id.clone(),
        snapshot_revision: Revision(graph.cas.component.revision),
        thread_refs: binding.threads.clone(),
        binding,
        restart_phase: phase(graph.cas.phase),
        disposition: disposition(graph.cas.component.disposition),
        claim,
        control,
        eligibility,
        resume,
        release,
        progress: progress(graph),
        thread_count: Revision(thread_count as u64),
        thread_refs_truncated: thread_count > 64,
        operation_limit: if thread_count > 64 {
            OperationLimitView::TooManyThreads { limit: 64 }
        } else {
            OperationLimitView::WithinLimit
        },
    };
    result.validate().map_err(|_| unavailable())?;
    Ok(result)
}

/// Whether this core's own epoch adopted the graph (the sealed live inventory).
pub fn adopted_by(graph: &OwnedGraphSnapshot, live_epoch: Option<InstanceEpoch>) -> bool {
    graph.adopted_epoch == Some(graph.cas.successor_epoch)
        && live_epoch.is_some_and(|epoch| epoch.0 == graph.cas.successor_epoch)
}

/// A graph whose recipe is in a format this build does not execute. Only its
/// relational header is read; the recipe is never decoded, and nothing here is
/// authority. `restart` is absent for a manifest recorded outside a transaction.
pub fn is_legacy(header: &RecoveryGraphHeader) -> bool {
    !header.is_current()
}

/// A legacy graph worth showing: not yet discarded, and its component is
/// neither released nor resumed. A finished old graph needs no action.
pub fn legacy_listed(header: &RecoveryGraphHeader) -> bool {
    is_legacy(header)
        && !header.superseded
        && header.component.as_ref().is_none_or(|c| {
            matches!(
                c.disposition,
                RestartDisposition::RetryPending | RestartDisposition::Quarantined
            )
        })
}

fn legacy_threads(header: &RecoveryGraphHeader) -> BTreeSet<&str> {
    header
        .bindings
        .iter()
        .map(|(thread, _)| thread.as_str())
        .collect()
}

fn legacy_ids(header: &RecoveryGraphHeader) -> (&str, &str) {
    header
        .restart
        .as_ref()
        .map_or(("", ""), |r| (r.restart_id.as_str(), r.capture_id.as_str()))
}

/// The binding a client quotes to discard a legacy graph. The recipe is never
/// read: the quoted revisions come from the relational header. The core epoch is
/// not recorded for such a graph and is quoted as 0.
pub fn legacy_binding_for(
    header: &RecoveryGraphHeader,
    operation_id: AdminId,
) -> Result<OwnedGraphBinding, AdminFailure> {
    let mut refs = GraphReferences::default();
    let (restart, capture) = legacy_ids(header);
    let manifest = &header.manifest_id;
    let graph_id = refs.graph_reference(restart, capture, manifest)?;
    let restart_tx_id = refs.graph_reference("transaction", restart, restart)?;
    let capture_id = refs.graph_reference("capture", restart, capture)?;
    let mut threads = Vec::new();
    for thread in legacy_threads(header) {
        threads.push(ThreadCas {
            thread_ref: refs.thread_reference(restart, capture, manifest, thread)?,
        });
    }
    threads.sort_by(|a, b| a.thread_ref.cmp(&b.thread_ref));
    threads.truncate(64);
    let binding = OwnedGraphBinding {
        graph: GraphCas {
            graph_id,
            restart_tx_id,
            capture_id,
            expected_revision: Revision(header.component.as_ref().map_or_else(
                || header.restart.as_ref().map_or(0, |r| r.revision),
                |c| c.revision,
            )),
            expected_core_epoch: InstanceEpoch(0),
            operation_id,
        },
        threads,
        expected_control_revision: Revision(0),
    };
    binding.validate().map_err(|_| unavailable())?;
    Ok(binding)
}

/// Mirrors what the storage writer accepts for an explicit discard: a graph of
/// an unfinished transaction is first isolated by the cold start, never here.
pub fn legacy_discardable(header: &RecoveryGraphHeader) -> bool {
    header.restart.as_ref().is_none_or(|r| {
        matches!(
            r.phase,
            RestartPhase::Invalidated
                | RestartPhase::Aborted
                | RestartPhase::Superseded
                | RestartPhase::Completed
        )
    }) && header
        .component
        .as_ref()
        .is_none_or(|c| c.disposition != RestartDisposition::RetryPending)
}

/// The display of a legacy graph: not executable, nothing of it is claimed to
/// be done, and its scratch is retained because relational membership is not
/// evidence that an external effect is ours.
pub fn legacy_detail(header: &RecoveryGraphHeader) -> Result<OwnedGraphView, AdminFailure> {
    let binding = legacy_binding_for(
        header,
        AdminId::new("unassigned").map_err(|_| unavailable())?,
    )?;
    let thread_count = legacy_threads(header).len();
    let release = if thread_count > 64 {
        blocked(GraphProblemDetail::TooManyThreads)
    } else if legacy_discardable(header) {
        ActionAvailability::Available
    } else {
        blocked(GraphProblemDetail::NeedsAttention)
    };
    let result = OwnedGraphView {
        graph_ref: binding.graph.graph_id.clone(),
        snapshot_revision: binding.graph.expected_revision,
        thread_refs: binding.threads.clone(),
        binding,
        // A manifest outside any transaction is shown as invalidated: it can
        // never run, and no transaction exists to be pending.
        restart_phase: header
            .restart
            .as_ref()
            .map_or(RestartPhaseView::Invalidated, |r| phase(r.phase)),
        disposition: header
            .component
            .as_ref()
            .map_or(RestartDispositionView::Quarantined, |c| {
                disposition(c.disposition)
            }),
        // The old recipe and its claim are not interpreted.
        claim: ClaimView::Unreadable,
        control: GraphControlView::Unknown,
        eligibility: GraphEligibilityView::ColdIsolated,
        resume: blocked(GraphProblemDetail::Cold),
        release,
        progress: DisposalProgressView {
            eligibility: StepObservation::NotStarted,
            prepared_release: StepObservation::NotStarted,
            scratch: StepObservation::NotApplicable,
            completion: DisposalCompletion::NotStarted,
        },
        thread_count: Revision(thread_count as u64),
        thread_refs_truncated: thread_count > 64,
        operation_limit: if thread_count > 64 {
            OperationLimitView::TooManyThreads { limit: 64 }
        } else {
            OperationLimitView::WithinLimit
        },
    };
    result.validate().map_err(|_| unavailable())?;
    Ok(result)
}

pub(super) fn legacy_reference(
    header: &RecoveryGraphHeader,
) -> Result<GraphReference, AdminFailure> {
    let mut refs = GraphReferences::default();
    let (restart, capture) = legacy_ids(header);
    Ok(GraphReference {
        graph_id: refs.graph_reference(restart, capture, &header.manifest_id)?,
        restart_tx_id: refs.graph_reference("transaction", restart, restart)?,
        capture_id: refs.graph_reference("capture", restart, capture)?,
        revision: Revision(header.component.as_ref().map_or_else(
            || header.restart.as_ref().map_or(0, |r| r.revision),
            |c| c.revision,
        )),
    })
}

pub(super) fn next_action(
    view: &OwnedGraphView,
    effect_unknown: bool,
    automatic_retry_pending: bool,
) -> NextAction {
    if effect_unknown {
        return NextAction::ObserveSameOperation;
    }
    if view.claim == ClaimView::Activated || view.disposition == RestartDispositionView::Activated {
        return NextAction::NormalStopRequired;
    }
    if view.progress.completion == DisposalCompletion::Complete {
        return NextAction::None;
    }
    if matches!(view.resume, ActionAvailability::Available) {
        return NextAction::RefreshAndConfirmResume;
    }
    if view.claim == ClaimView::Absent {
        if !matches!(view.release, ActionAvailability::Available) {
            return NextAction::NormalStopRequired;
        }
        return if view.control == GraphControlView::Open
            && view.disposition == RestartDispositionView::RetryPending
            && automatic_retry_pending
        {
            NextAction::WaitAutomaticRetryOrRelease
        } else {
            NextAction::ReleaseOnly
        };
    }
    if matches!(view.release, ActionAvailability::Available) {
        NextAction::RefreshAndConfirmRelease
    } else {
        NextAction::None
    }
}

#[derive(Clone, PartialEq, Eq)]
pub(super) struct GraphFact {
    pub reference: GraphReference,
    pub undisposed: bool,
    pub incomplete: bool,
    pub over_limit: bool,
    pub threads: usize,
}
pub(super) fn graph_fact(
    graph: &OwnedGraphSnapshot,
    live_epoch: Option<InstanceEpoch>,
) -> Result<GraphFact, AdminFailure> {
    let complete = progress(graph).completion == DisposalCompletion::Complete;
    let resumed = graph
        .cas
        .claim
        .as_ref()
        .is_some_and(|c| c.state == RecoveryClaimState::Activated)
        || graph.cas.component.disposition == RestartDisposition::Activated;
    let revoked = if graph.control.as_ref().is_some_and(|c| c.revoked) {
        RevocationObservation::Confirmed
    } else {
        RevocationObservation::NotApplied
    };
    let mut refs = GraphReferences::default();
    let cas = &graph.cas;
    let reference = GraphReference {
        graph_id: refs.graph_reference(
            &cas.restart_id,
            &cas.capture_id,
            &cas.manifest.manifest_id,
        )?,
        restart_tx_id: refs.graph_reference("transaction", &cas.restart_id, &cas.restart_id)?,
        capture_id: refs.graph_reference("capture", &cas.restart_id, &cas.capture_id)?,
        revision: Revision(cas.component.revision),
    };
    let threads = thread_ids(cas).len();
    let facts = GraphDisposalFacts {
        verified_live_adoption: adopted_by(graph, live_epoch),
        thread_count: threads,
        resume_committed: resumed,
        disposal_complete: complete,
        revocation: revoked,
    };
    Ok(GraphFact {
        reference,
        undisposed: facts.is_undisposed(),
        incomplete: facts.is_incomplete_disposal(),
        over_limit: threads > 64,
        threads,
    })
}
/// `legacy` are graphs in a format this build does not execute. They are
/// listed with the warnings (never as restart blockers) so that a person can
/// find and explicitly discard them.
pub(super) fn aggregate_facts<'a>(
    facts: impl Iterator<Item = &'a GraphFact>,
    legacy: impl Iterator<Item = &'a GraphReference>,
    revision: Revision,
) -> Result<UndisposedGraphs, AdminFailure> {
    let mut references = Vec::new();
    let mut incomplete = Vec::new();
    let mut over_limit = 0;
    for fact in facts {
        if fact.undisposed {
            references.push(fact.reference.clone());
            if fact.over_limit {
                over_limit += 1;
            }
        }
        if fact.incomplete {
            incomplete.push(fact.reference.clone());
        }
    }
    incomplete.extend(legacy.cloned());
    references.sort_by(|a, b| a.graph_id.cmp(&b.graph_id));
    incomplete.sort_by(|a, b| a.graph_id.cmp(&b.graph_id));
    let count = references.len();
    let warnings = incomplete.len();
    references.truncate(64);
    incomplete.truncate(64);
    let result = UndisposedGraphs {
        count: Revision(count as u64),
        references,
        snapshot_revision: revision,
        truncated: count > 64,
        over_limit_count: Revision(over_limit),
        incomplete_disposals: IncompleteDisposals {
            count: Revision(warnings as u64),
            references: incomplete,
            truncated: warnings > 64,
        },
    };
    result.validate().map_err(|_| unavailable())?;
    Ok(result)
}
/// Read-only diagnostic helper. Admission exclusively uses LiveGraphCollector.
pub fn collect_undisposed(
    graphs: &[OwnedGraphSnapshot],
    legacy: &[RecoveryGraphHeader],
    live_epoch: Option<InstanceEpoch>,
) -> Result<UndisposedGraphs, AdminFailure> {
    let facts = graphs
        .iter()
        .filter(|g| adopted_by(g, live_epoch))
        .map(|g| graph_fact(g, live_epoch))
        .collect::<Result<Vec<_>, _>>()?;
    let legacy = legacy
        .iter()
        .filter(|h| legacy_listed(h))
        .map(legacy_reference)
        .collect::<Result<Vec<_>, _>>()?;
    aggregate_facts(facts.iter(), legacy.iter(), Revision(0))
}
