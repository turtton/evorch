use super::*;
use storage::recovery::{
    RECOVERY_SCHEMA_VERSION, RecoveryManifestRecord, RecoveryMemberRecord, RestartHeader,
    restart::*,
};

fn graph(n: usize) -> OwnedGraphSnapshot {
    let membership = (0..n)
        .map(|i| RestartExpectedMember {
            run_id: runtime::RunId::new(i as u64 + 1).to_string(),
            incarnation: "old".into(),
            root_run_id: runtime::RunId::new(i as u64 + 1).to_string(),
            thread_id: Some(format!("/private/thread-{i}")),
            intent_revision: 0,
            snapshot: None,
            lane_id: None,
        })
        .collect::<Vec<_>>();
    OwnedGraphSnapshot {
        cas: GraphControlCas {
            restart_id: "tx".into(),
            capture_id: "capture".into(),
            successor_epoch: 2,
            phase: RestartPhase::Adopted,
            component: RestartComponent {
                manifest_id: "/private/manifest".into(),
                disposition: RestartDisposition::RetryPending,
                claim_id: None,
                reason: None,
                revision: 1,
            },
            manifest: RecoveryManifestRecord {
                manifest_id: "/private/manifest".into(),
                schema_version: RECOVERY_SCHEMA_VERSION,
                committed: true,
                superseded: false,
                edges: vec![],
                members: membership
                    .iter()
                    .map(|m| RecoveryMemberRecord {
                        run_id: m.run_id.clone(),
                        incarnation: m.incarnation.clone(),
                        intent_revision: 0,
                        snapshot: None,
                        resume: true,
                        record_json: "{}".into(),
                    })
                    .collect(),
            },
            membership,
            claim: None,
            control_revision: 0,
            control_effect: None,
        },
        adopted_epoch: None,
        control: None,
        progress: vec![],
    }
}
#[test]
fn cold_and_over_limit_views_are_bounded_read_only_and_path_free() {
    for n in [1, 64, 65] {
        let snapshot = graph(n);
        let view = detail(&snapshot, Some(InstanceEpoch(2))).unwrap();
        assert_eq!(view.thread_count, Revision(n as u64));
        assert_eq!(view.thread_refs.len(), n.min(64));
        assert_eq!(view.thread_refs_truncated, n > 64);
        assert_eq!(view.eligibility, GraphEligibilityView::ColdIsolated);
        assert!(!matches!(view.resume, ActionAvailability::Available));
        assert!(!matches!(view.release, ActionAvailability::Available));
        if n > 64 {
            assert_eq!(
                view.operation_limit,
                OperationLimitView::TooManyThreads { limit: 64 }
            );
            assert!(matches!(
                view.release,
                ActionAvailability::Blocked {
                    detail: GraphProblemDetail::TooManyThreads,
                    ..
                }
            ));
        }
        assert!(!serde_json::to_string(&view).unwrap().contains("private"));
    }
}
#[test]
fn an_adopted_graph_of_this_epoch_can_be_released_and_resumed_only_while_prepared() {
    let mut snapshot = graph(2);
    snapshot.adopted_epoch = Some(snapshot.cas.successor_epoch);
    let view = detail(&snapshot, Some(InstanceEpoch(2))).unwrap();
    // Adopted by an earlier core, or no live epoch yet: cold.
    for epoch in [Some(InstanceEpoch(3)), None] {
        assert_eq!(
            detail(&snapshot, epoch).unwrap().eligibility,
            GraphEligibilityView::ColdIsolated
        );
    }
    assert_eq!(view.eligibility, GraphEligibilityView::AdoptedRetryPending);
    assert!(matches!(view.release, ActionAvailability::Available));
    // No claim: there is nothing to resume.
    assert!(matches!(
        view.resume,
        ActionAvailability::Blocked {
            detail: GraphProblemDetail::NoClaim,
            ..
        }
    ));
}
#[test]
fn no_claim_guidance_requires_current_automatic_retry_membership() {
    let mut view = detail(&graph(1), Some(InstanceEpoch(2))).unwrap();
    for automatic in [false, true] {
        assert_eq!(
            next_action(&view, false, automatic),
            NextAction::NormalStopRequired
        );
    }
    // Automatic membership only changes guidance when Release is authorized.
    view.release = ActionAvailability::Available;
    assert_eq!(next_action(&view, false, false), NextAction::ReleaseOnly);
    assert_eq!(
        next_action(&view, false, true),
        NextAction::WaitAutomaticRetryOrRelease
    );
    view.control = GraphControlView::ManualIdle;
    assert_eq!(next_action(&view, false, true), NextAction::ReleaseOnly);
    assert_eq!(
        next_action(&view, true, true),
        NextAction::ObserveSameOperation
    );
}

#[test]
fn activated_resume_query_ignores_the_old_running_effect_journal() {
    let mut graph = detail(&graph(1), Some(InstanceEpoch(2))).unwrap();
    graph.claim = ClaimView::Activated;
    graph.disposition = RestartDispositionView::Activated;
    assert_eq!(
        super::super::facade::operation_next_action(&graph, "manual_running", true, false),
        NextAction::NormalStopRequired
    );
    graph.claim = ClaimView::Prepared;
    graph.disposition = RestartDispositionView::RetryPending;
    assert_eq!(
        super::super::facade::operation_next_action(&graph, "manual_running", false, false),
        NextAction::ObserveSameOperation
    );
}

fn legacy_header(
    phase: Option<RestartPhase>,
    disposition: Option<RestartDisposition>,
) -> RecoveryGraphHeader {
    RecoveryGraphHeader {
        manifest_id: "/private/manifest".into(),
        schema_version: 1,
        committed: true,
        superseded: false,
        restart: phase.map(|phase| RestartHeader {
            restart_id: "tx".into(),
            capture_id: "capture".into(),
            format_version: 1,
            revision: 3,
            phase,
        }),
        component: disposition.map(|disposition| RestartComponent {
            manifest_id: "/private/manifest".into(),
            disposition,
            claim_id: None,
            reason: None,
            revision: 5,
        }),
        bindings: vec![("/private/thread-a".into(), Some("run-a".into()))],
    }
}
#[test]
fn a_legacy_graph_is_listed_by_its_header_and_never_claimed_executable() {
    let header = legacy_header(
        Some(RestartPhase::Invalidated),
        Some(RestartDisposition::Quarantined),
    );
    assert!(is_legacy(&header) && legacy_listed(&header));
    let view = legacy_detail(&header).unwrap();
    assert_eq!(view.claim, ClaimView::Unreadable);
    assert_eq!(view.eligibility, GraphEligibilityView::ColdIsolated);
    assert!(matches!(
        view.resume,
        ActionAvailability::Blocked {
            detail: GraphProblemDetail::Cold,
            ..
        }
    ));
    assert!(matches!(view.release, ActionAvailability::Available));
    // Nothing is claimed to be cleaned: its scratch is retained.
    assert_eq!(view.progress.scratch, StepObservation::NotApplicable);
    assert_eq!(view.progress.completion, DisposalCompletion::NotStarted);
    assert_eq!(view.thread_count, Revision(1));
    assert!(!serde_json::to_string(&view).unwrap().contains("private"));
    // The reference the list shows is the one the detail and binding quote.
    assert_eq!(legacy_reference(&header).unwrap().graph_id, view.graph_ref);
    assert_eq!(
        legacy_binding_for(&header, AdminId::new("op").unwrap())
            .unwrap()
            .graph
            .graph_id,
        view.graph_ref
    );
}
#[test]
fn a_legacy_graph_of_an_unfinished_transaction_waits_for_the_cold_isolation() {
    let pending = legacy_header(
        Some(RestartPhase::Adopted),
        Some(RestartDisposition::RetryPending),
    );
    assert!(legacy_listed(&pending) && !legacy_discardable(&pending));
    assert!(matches!(
        legacy_detail(&pending).unwrap().release,
        ActionAvailability::Blocked {
            detail: GraphProblemDetail::NeedsAttention,
            ..
        }
    ));
    // A manifest outside any transaction can be discarded at once.
    assert!(legacy_discardable(&legacy_header(None, None)));
}
#[test]
fn discarded_released_and_current_graphs_are_not_listed_as_legacy() {
    let mut discarded = legacy_header(
        Some(RestartPhase::Invalidated),
        Some(RestartDisposition::Released),
    );
    assert!(!legacy_listed(&discarded));
    discarded.component = Some(RestartComponent {
        disposition: RestartDisposition::Quarantined,
        ..discarded.component.clone().unwrap()
    });
    discarded.superseded = true;
    assert!(!legacy_listed(&discarded));
    let mut current = legacy_header(None, None);
    current.schema_version = RECOVERY_SCHEMA_VERSION;
    assert!(!is_legacy(&current) && !legacy_listed(&current));
}
