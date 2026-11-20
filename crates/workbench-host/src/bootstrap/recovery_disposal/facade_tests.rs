use super::*;

fn check(error: runtime::RuntimeError, expected: AdminOutcome, retained: bool) {
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
            panic!("a disposal refusal cannot request a provider")
        }
    }
    let bus = Arc::new(event_bus::EventBus::new(16));
    let runtime = AgentRuntime::new(
        bus.clone(),
        Arc::new(::tools::ToolExecutor::new(bus)),
        Arc::new(Never),
    );
    let effect = GraphEffectKey {
        operation_id: "first".into(),
        attempt_nonce: "nonce".into(),
    };
    let job = Arc::new(UncertainDisposal {
        reservation: runtime
            .reserve_owned_graph_disposal("graph", effect.clone(), 0)
            .unwrap(),
        effect,
        finished: std::sync::atomic::AtomicBool::new(true),
    });
    let uncertain = Mutex::new(BTreeMap::from([("first".into(), job.clone())]));
    assert_eq!(finish_disposal(&uncertain, job, Err(error)), expected);
    assert_eq!(uncertain.lock().unwrap().len(), usize::from(retained));
    let next = runtime.reserve_owned_graph_disposal(
        "graph",
        GraphEffectKey {
            operation_id: "next".into(),
            attempt_nonce: "next".into(),
        },
        0,
    );
    assert_eq!(next.is_ok(), !retained);
    assert!(runtime.list_agents().is_empty());
}

#[test]
fn disposal_conflict_releases_original_reservation() {
    check(
        runtime::RuntimeError::GraphControlConflict,
        AdminOutcome::Conflict,
        false,
    );
}
#[test]
fn disposal_confirmed_not_applied_releases_original_reservation() {
    check(
        runtime::RuntimeError::GraphControlNotApplied,
        AdminOutcome::Failed,
        false,
    );
}
#[test]
fn disposal_unknown_keeps_original_reservation() {
    check(
        runtime::RuntimeError::GraphControlUnknown,
        AdminOutcome::Unknown,
        true,
    );
}
