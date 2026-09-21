use event_bus::{Event, ProviderEvent, ProviderFailureKind};
use gui::{app::WorkbenchState, headless::HeadlessWorkbench, model::tasks::AgentRunSource};

struct EmptySource;
impl AgentRunSource for EmptySource {
    fn list(&self) -> Vec<runtime::AgentSummary> {
        Vec::new()
    }
}

#[test]
fn fallback_notice_is_visible_in_real_conversation() {
    // Given
    let mut state = WorkbenchState::new(EmptySource, &workspace_ui::UiSettings::default()).unwrap();
    #[path = "support/thread_root.rs"]
    mod thread_root;
    thread_root::bind_root(&mut state, "session");
    // When
    state.apply_events([Event::new(ProviderEvent::FallbackTriggered {
        from_provider: "kimi".into(),
        from_model: Some("k3".into()),
        to_provider: "neuralwatt".into(),
        to_model: "kimi-k3".into(),
        logical_model: "worker".into(),
        session_id: "session".into(),
        failure: ProviderFailureKind::Timeout,
        request_id: None,
    })]);
    let mut harness = HeadlessWorkbench::new(state, [1200.0, 900.0]);
    harness.run();
    // Then
    assert!(harness.has_label("Fallback: kimi/k3 → neuralwatt/kimi-k3 (previous provider failed)"));
}
