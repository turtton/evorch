use super::*;
use std::sync::Arc;
use storage::recovery::{RecoveryManifestRecord, RecoveryMemberRecord, restart::*};

fn graph(n: usize) -> OwnedGraphSnapshot {
    let membership = (0..n)
        .map(|i| RestartExpectedMember {
            run_id: runtime::RunId::new(i as u64 + 1).to_string(),
            incarnation: "old".into(),
            root_run_id: runtime::RunId::new(i as u64 + 1).to_string(),
            thread_id: Some(format!("/private/thread-{i}")),
            intent_revision: 0,
            snapshot: None,
            owner_id: None,
            owner_generation: None,
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
                schema_version: 1,
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
fn runtime() -> AgentRuntime {
    struct Never;
    #[async_trait::async_trait]
    impl runtime::AgentModel for Never {
        fn selected_model(&self, _: runtime::Role, _: Option<&str>) -> String {
            "never".into()
        }
        async fn complete(
            &self,
            _: &runtime::AgentInvocationContext,
            _: runtime::Role,
            _: &[providers::Message],
            _: &[providers::ToolSpec],
        ) -> Result<providers::ChatResponse, runtime::RuntimeError> {
            panic!("a read model must not request a provider");
        }
    }
    let bus = Arc::new(event_bus::EventBus::new(16));
    AgentRuntime::new(
        bus.clone(),
        Arc::new(::tools::ToolExecutor::new(bus)),
        Arc::new(Never),
    )
}
#[test]
fn cold_and_over_limit_views_are_bounded_read_only_and_path_free() {
    let dir = test_socket_dir::SocketDir::new();
    let bus = Arc::new(event_bus::EventBus::new(16));
    let owner = OwnerHost::open_planned(dir.path(), Default::default(), bus).unwrap();
    let runtime = runtime();
    for n in [1, 64, 65] {
        let snapshot = graph(n);
        let view = detail(&runtime, &owner, &snapshot).unwrap();
        assert_eq!(view.owner_count, Revision(n as u64));
        assert_eq!(view.owner_refs.len(), n.min(64));
        assert_eq!(view.owner_refs_truncated, n > 64);
        assert_eq!(view.ownership, OwnershipView::Cold);
        assert!(!matches!(view.resume, ActionAvailability::Available));
        assert!(!matches!(view.release, ActionAvailability::Available));
        if n > 64 {
            assert_eq!(
                view.operation_limit,
                OperationLimitView::TooManyOwners { limit: 64 }
            );
            assert!(matches!(
                view.release,
                ActionAvailability::Blocked {
                    detail: GraphProblemDetail::TooManyOwners,
                    ..
                }
            ));
        }
        assert!(!serde_json::to_string(&view).unwrap().contains("private"));
        assert!(owner.restart_inventory().unwrap().is_empty());
        assert!(runtime.list_agents().is_empty());
    }
    owner.stop_worker_for_exec().unwrap();
}
#[test]
fn no_claim_guidance_requires_current_automatic_retry_membership() {
    let dir = test_socket_dir::SocketDir::new();
    let owner = OwnerHost::open_planned(
        dir.path(),
        Default::default(),
        Arc::new(event_bus::EventBus::new(16)),
    )
    .unwrap();
    let mut view = detail(&runtime(), &owner, &graph(1)).unwrap();
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
    owner.stop_worker_for_exec().unwrap();
}

#[test]
fn activated_resume_query_ignores_the_old_running_effect_journal() {
    let dir = test_socket_dir::SocketDir::new();
    let owner = OwnerHost::open_planned(
        dir.path(),
        Default::default(),
        Arc::new(event_bus::EventBus::new(16)),
    )
    .unwrap();
    let mut graph = detail(&runtime(), &owner, &graph(1)).unwrap();
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
    owner.stop_worker_for_exec().unwrap();
}
