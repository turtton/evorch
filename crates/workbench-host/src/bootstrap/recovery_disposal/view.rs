use crate::admin::{
    self, AdminFailure, GraphOwnershipSummary, GraphReferences, RevocationObservation,
};
use runtime::{
    AgentRuntime, OwnedGraphOwnershipState, OwnedThreadCas,
    ownership::{OwnerHost, RegistryError},
};
use std::collections::{BTreeMap, BTreeSet};
use storage::recovery::{
    RecoveryClaimState,
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

/// Complete raw expectations remain server-side. Neither returned value grants
/// execution/disposal authority; the operation resolves its own typed authority.
pub fn binding_for(
    owner: &OwnerHost,
    cas: &GraphControlCas,
    operation_id: AdminId,
) -> Result<(OwnedGraphBinding, Vec<OwnedThreadCas>), AdminFailure> {
    let mut refs = GraphReferences::default();
    let manifest = &cas.manifest.manifest_id;
    let graph_id = refs.graph_reference(&cas.restart_id, &cas.capture_id, manifest)?;
    let restart_tx_id = refs.graph_reference("transaction", &cas.restart_id, &cas.restart_id)?;
    let capture_id = refs.graph_reference("capture", &cas.restart_id, &cas.capture_id)?;
    let mut raw = Vec::new();
    let mut owners = Vec::new();
    for thread in cas
        .membership
        .iter()
        .filter_map(|m| m.thread_id.clone())
        .collect::<BTreeSet<_>>()
    {
        let row = match owner.attach(&thread) {
            Ok(row) => Some(row),
            Err(RegistryError::Absent) => None,
            Err(_) => return Err(unavailable()),
        };
        raw.push(OwnedThreadCas {
            thread_id: thread.clone(),
            owner_identity: row.as_ref().map(|r| r.lease.owner_id.clone()),
            owner_generation: row.as_ref().map(|r| r.lease.generation),
        });
        let owner_identity = row
            .as_ref()
            .map(|r| refs.graph_reference("owner", &r.lease.owner_id, &r.lease.owner_id))
            .transpose()?;
        owners.push(OwnerCas {
            thread_ref: refs.thread_reference(
                &cas.restart_id,
                &cas.capture_id,
                manifest,
                &thread,
            )?,
            owner_identity,
            owner_generation: row.map(|r| Revision(r.lease.generation)),
        });
    }
    owners.sort_by(|a, b| a.thread_ref.cmp(&b.thread_ref));
    owners.truncate(64);
    let binding = OwnedGraphBinding {
        graph: GraphCas {
            graph_id,
            restart_tx_id,
            capture_id,
            expected_revision: Revision(cas.component.revision),
            expected_core_epoch: InstanceEpoch(cas.successor_epoch),
            owner_identity: owners.first().and_then(|o| o.owner_identity.clone()),
            owner_generation: owners.first().and_then(|o| o.owner_generation),
            operation_id,
        },
        owners,
        expected_control_revision: Revision(cas.control_revision),
    };
    binding.validate().map_err(|_| unavailable())?;
    Ok((binding, raw))
}

fn ownership(state: OwnedGraphOwnershipState) -> OwnershipView {
    match state {
        OwnedGraphOwnershipState::LiveSelf => OwnershipView::LiveSelf,
        OwnedGraphOwnershipState::MixedSelfAndAbsent => OwnershipView::MixedSelfAndAbsent,
        OwnedGraphOwnershipState::Absent => OwnershipView::Absent,
        OwnedGraphOwnershipState::Foreign => OwnershipView::Foreign,
        OwnedGraphOwnershipState::GenerationChanged => OwnershipView::GenerationChanged,
        OwnedGraphOwnershipState::Unavailable => OwnershipView::Unknown,
        OwnedGraphOwnershipState::Cold => OwnershipView::Cold,
    }
}

pub(super) fn progress(graph: &OwnedGraphSnapshot) -> DisposalProgressView {
    let revoked = graph.control.as_ref().is_some_and(|c| c.revoked);
    let threads = graph
        .cas
        .membership
        .iter()
        .filter_map(|m| m.thread_id.as_ref())
        .collect::<BTreeSet<_>>();
    let n = threads.len();
    let mut latest = BTreeMap::<_, &storage::recovery::graph_progress::GraphThreadProgress>::new();
    for p in &graph.progress {
        if !threads.contains(&p.key.thread_id) {
            continue;
        }
        let current = latest.entry(&p.key.thread_id).or_insert(p);
        if (p.key.owner_generation, p.revision) > (current.key.owner_generation, current.revision) {
            *current = p;
        }
    }
    let aggregate = |owner: bool| {
        let steps = latest
            .values()
            .map(|p| if owner { p.owners } else { p.scratch })
            .collect::<Vec<_>>();
        if steps.contains(&GraphDisposalStep::Unknown) {
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
        }
    };
    let owners = aggregate(true);
    let scratch = aggregate(false);
    let done = |s| {
        matches!(
            s,
            StepObservation::ConfirmedApplied | StepObservation::NotApplicable
        )
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
        owners,
        scratch,
        completion: if !revoked {
            DisposalCompletion::NotStarted
        } else if done(owners) && done(scratch) {
            DisposalCompletion::Complete
        } else if owners == StepObservation::Unknown || scratch == StepObservation::Unknown {
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
        problem: GraphAuthorityProblem::OwnerBlocked,
        detail,
    }
}
/// Project independent persisted facts, not a second lifecycle state machine.
/// Execution authority is separately checked again inside the core port.
pub fn detail(
    runtime: &AgentRuntime,
    owner: &OwnerHost,
    graph: &OwnedGraphSnapshot,
) -> Result<OwnedGraphView, AdminFailure> {
    let (binding, raw) = binding_for(
        owner,
        &graph.cas,
        AdminId::new("unassigned").map_err(|_| unavailable())?,
    )?;
    let state = if graph.adopted_epoch == Some(graph.cas.successor_epoch) {
        runtime
            .owned_graph_ownership_diagnostic(&graph.cas)
            .map_err(|_| unavailable())?
            .state()
    } else {
        OwnedGraphOwnershipState::Cold
    };
    if state == OwnedGraphOwnershipState::Unavailable {
        return Err(unavailable());
    }
    let own = ownership(state);
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
    } else if own == OwnershipView::Cold {
        GraphEligibilityView::ColdIsolated
    } else if graph.cas.component.disposition == RestartDisposition::RetryPending {
        GraphEligibilityView::AdoptedRetryPending
    } else {
        GraphEligibilityView::NeedsAttention
    };
    let release = if raw.len() > 64 {
        blocked(GraphProblemDetail::TooManyOwners)
    } else if preadoption {
        blocked(GraphProblemDetail::PreAdoption)
    } else if activated {
        blocked(GraphProblemDetail::AlreadyActivated)
    } else if (terminal || graph.cas.phase == RestartPhase::Completed)
        && claim == ClaimView::Prepared
    {
        blocked(GraphProblemDetail::NeedsAttention)
    } else if !matches!(
        own,
        OwnershipView::LiveSelf | OwnershipView::MixedSelfAndAbsent
    ) {
        blocked(match own {
            OwnershipView::Foreign => GraphProblemDetail::ForeignOwner,
            OwnershipView::GenerationChanged => GraphProblemDetail::GenerationChanged,
            OwnershipView::Absent => GraphProblemDetail::OwnerAbsent,
            _ => GraphProblemDetail::Cold,
        })
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
    } else if own != OwnershipView::LiveSelf {
        blocked(GraphProblemDetail::OwnerAbsent)
    } else {
        ActionAvailability::Available
    };
    let owner_refs = binding
        .owners
        .iter()
        .map(|o| OwnerView {
            binding: o.clone(),
            ownership: if o.owner_identity.is_none() {
                OwnershipView::Absent
            } else {
                own
            },
            cleanup: StepObservation::NotStarted,
        })
        .collect();
    let result = OwnedGraphView {
        graph_ref: binding.graph.graph_id.clone(),
        snapshot_revision: Revision(graph.cas.component.revision),
        binding,
        restart_phase: phase(graph.cas.phase),
        disposition: disposition(graph.cas.component.disposition),
        claim,
        ownership: own,
        control,
        eligibility,
        resume,
        release,
        progress: progress(graph),
        owner_count: Revision(raw.len() as u64),
        owner_refs,
        owner_refs_truncated: raw.len() > 64,
        operation_limit: if raw.len() > 64 {
            OperationLimitView::TooManyOwners { limit: 64 }
        } else {
            OperationLimitView::WithinLimit
        },
    };
    result.validate().map_err(|_| unavailable())?;
    Ok(result)
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

pub fn collect_undisposed(
    runtime: &AgentRuntime,
    owner: &OwnerHost,
    graphs: &[OwnedGraphSnapshot],
) -> Result<UndisposedGraphs, AdminFailure> {
    let mut references = Vec::new();
    let mut incomplete = Vec::new();
    let mut over_limit = 0;
    let mut revision = 0u64;
    for graph in graphs {
        revision = revision
            .checked_add(graph.cas.component.revision)
            .and_then(|r| r.checked_add(graph.cas.control_revision))
            .ok_or_else(unavailable)?;
        if graph.adopted_epoch != Some(graph.cas.successor_epoch) {
            continue;
        }
        let diagnostic = runtime
            .owned_graph_ownership_diagnostic(&graph.cas)
            .map_err(|_| unavailable())?;
        let summary = GraphOwnershipSummary::try_from(&diagnostic)?;
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
        let undisposed = summary.is_undisposed(resumed, complete, revoked);
        let warning = summary.is_incomplete_disposal(complete, revoked);
        if undisposed || warning {
            let (binding, _) = binding_for(
                owner,
                &graph.cas,
                AdminId::new("unassigned").map_err(|_| unavailable())?,
            )?;
            let g = binding.graph;
            let reference = GraphReference {
                graph_id: g.graph_id,
                restart_tx_id: g.restart_tx_id,
                capture_id: g.capture_id,
                revision: g.expected_revision,
            };
            if undisposed {
                if diagnostic.owner_count() > 64 {
                    over_limit += 1;
                }
                references.push(reference.clone());
            }
            if warning {
                incomplete.push(reference);
            }
        }
    }
    references.sort_by(|a, b| a.graph_id.cmp(&b.graph_id));
    incomplete.sort_by(|a, b| a.graph_id.cmp(&b.graph_id));
    let count = references.len();
    let warnings = incomplete.len();
    references.truncate(64);
    incomplete.truncate(64);
    let result = UndisposedGraphs {
        count: Revision(count as u64),
        references,
        snapshot_revision: Revision(revision),
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
