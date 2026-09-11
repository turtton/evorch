#[test]
#[ignore = "requires offscreen rendering adapter"]
fn capture_cancel_states() {
    // Given: an active thread in each composer evidence state.
    let temp = tempfile::tempdir().expect("temp directory");
    for (name, phase, text) in [
        ("running", event_bus::AgentRunPhase::Running, "実行中の下書き"),
        ("waiting", event_bus::AgentRunPhase::Waiting, "入力待ち"),
        ("completion", event_bus::AgentRunPhase::Running, "/"),
    ] {
        let mut state = crate::app::WorkbenchState::new(
            crate::fixture::DemoSource(Vec::new()),
            &workspace_ui::UiSettings::default(),
        )
        .expect("workbench")
        .with_provider_status(crate::model::composer::ProviderStatus::Configured);
        state.add_project(temp.path()).expect("project");
        state.create_thread(name).expect("thread");
        state.composer_mut().input = text.into();
        state.apply_events([
            event_bus::Event::new(event_bus::LifecycleEvent::AgentRunStarted {
                run_id: name.into(),
                parent_run_id: None,
                agent_name: "worker".into(),
                role: "worker".into(),
            }),
            event_bus::Event::new(event_bus::LifecycleEvent::AgentRunStateChanged {
                run_id: name.into(),
                from: event_bus::AgentRunPhase::Pending,
                to: phase,
                reason: None,
            }),
        ]);
        let mut h = crate::headless::HeadlessWorkbench::new(state, [800.0, 600.0]);
        h.run();
        // When: evidence is captured under the shared adapter policy.
        let Some(frame) = crate::evidence::capture_or_skip(&mut h) else {
            return;
        };
        // Then: the original evidence path is written.
        frame
            .save_png(std::path::Path::new(&format!("/tmp/opencode/e-cancel-{name}.png")))
            .expect("save evidence");
    }
}
