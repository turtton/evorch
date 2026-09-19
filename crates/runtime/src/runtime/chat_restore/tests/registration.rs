use super::*;

struct AdmissionModel {
    reject: AtomicBool,
}

#[async_trait::async_trait]
impl AgentModel for AdmissionModel {
    fn requires_admission(&self) -> bool {
        true
    }

    async fn admit(&self, _: &crate::AgentInvocationContext, _: Role) -> Result<(), RuntimeError> {
        if self.reject.load(Ordering::Relaxed) {
            return Err(RuntimeError::Model {
                reason: "admission rejected".into(),
            });
        }
        Ok(())
    }

    async fn complete(
        &self,
        context: &crate::AgentInvocationContext,
        role: Role,
        messages: &[providers::Message],
        tools: &[providers::ToolSpec],
    ) -> Result<providers::ChatResponse, RuntimeError> {
        CompletingModel
            .complete(context, role, messages, tools)
            .await
    }

    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "test".into()
    }
}

#[tokio::test]
async fn restored_event_observes_registered_entry_before_execution() {
    // Given: a terminal goal on a current-thread executor.
    let fixture = Fixture::new();
    let run = fixture.terminal().await;
    let mut events = fixture.runtime.shared.bus.subscribe();
    // When: continuing with new authority and receiving the first restore event.
    fixture
        .runtime
        .continue_goal(run, "continue".into(), RunConfig::default())
        .unwrap();
    let event = tokio::time::timeout(Duration::from_secs(2), events.recv())
        .await
        .unwrap()
        .unwrap();
    // Then: publication observes the new Pending entry, never the old terminal entry.
    assert!(
        matches!(event.kind, event_bus::EventKind::Lifecycle(LifecycleEvent::AgentRunRestored {
        run_id, restored_by, message_id
    }) if run_id == run.to_string() && restored_by == "user" && !message_id.is_empty())
    );
    let entry = fixture.runtime.entry(run).unwrap();
    assert_eq!(*entry.phase_rx.borrow(), AgentRunPhase::Pending);
    assert_eq!(entry.config.network_access, agents::NetworkAccess::Denied);
    assert!(entry.config.interactive && entry.config.keep_alive);
}

#[tokio::test]
async fn restored_event_is_absent_when_admission_fails() {
    // Given: a terminal goal and a provider that rejects its next admission.
    let model = Arc::new(AdmissionModel {
        reject: AtomicBool::new(false),
    });
    let fixture = Fixture::with_model(model.clone());
    let run = fixture.terminal().await;
    model.reject.store(true, Ordering::Relaxed);
    let mut events = fixture.runtime.shared.bus.subscribe();
    // When: the continuation fails admission.
    fixture
        .runtime
        .continue_goal(run, "continue".into(), RunConfig::default())
        .unwrap();
    assert!(fixture.runtime.wait_admission(run).await.is_err());
    // Then: no restore was announced and the old entry remains terminal.
    assert!(
        tokio::time::timeout(Duration::from_millis(20), events.recv())
            .await
            .is_err()
    );
    assert_eq!(
        *fixture.runtime.entry(run).unwrap().phase_rx.borrow(),
        AgentRunPhase::Done
    );
}
