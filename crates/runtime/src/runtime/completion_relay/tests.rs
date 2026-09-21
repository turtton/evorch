use super::*;

struct PendingModel;

#[async_trait::async_trait]
impl AgentModel for PendingModel {
    async fn complete(
        &self,
        _: &crate::AgentInvocationContext,
        _: Role,
        _: &[providers::Message],
        _: &[providers::ToolSpec],
    ) -> Result<providers::ChatResponse, RuntimeError> {
        std::future::pending().await
    }

    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "pending".into()
    }
}

#[tokio::test]
async fn terminal_replay_relays_once_without_final_output() {
    // Given: registered runs that have not produced any output.
    let bus = Arc::new(EventBus::new(128));
    let executor = Arc::new(ToolExecutor::with_standard_tools(
        Arc::clone(&bus),
        Arc::new(sandbox::DirectSandbox::new_unchecked()),
    ));
    let runtime = AgentRuntime::new(bus, executor, Arc::new(PendingModel));
    let parent = runtime.delegate_background(
        Role::Orchestrator,
        "private parent".into(),
        RunConfig::default(),
    );
    let child = runtime
        .delegate_background_as_child(parent, Role::Worker, "private child", RunConfig::default())
        .unwrap();
    let terminal = LifecycleEvent::AgentRunStateChanged {
        run_id: child.to_string(),
        from: AgentRunPhase::Running,
        to: AgentRunPhase::Done,
        reason: None,
    };
    // When: the terminal publication is replayed.
    runtime.publish_terminal(child, terminal.clone());
    runtime.publish_terminal(child, terminal);
    // Then: one metadata-only message survives both publications.
    let messages = runtime.take_inbox(parent).unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].content, "completed without final output");
    assert_eq!(messages[0].sender_run_id, child.to_string());
    assert_eq!(messages[0].recipient_run_id, parent.to_string());
    assert_eq!(messages[0].kind, AgentMessageKind::Send);
    assert_eq!(messages[0].reply_to, None);
}
