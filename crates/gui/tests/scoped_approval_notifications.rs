use egui_kittest::{Harness, kittest::Queryable};
use event_bus::{Event, ToolEvent};
use gui::{
    app::WorkbenchState, fixture::DemoSource, model::transcript_registry::TranscriptRegistry,
};
use workspace_ui::{PanelId, UiSettings};

#[test]
fn runtime_approval_formats_open_run_transcript_without_started_event() {
    // Given: executor preflight, network, and post-failure correlation IDs without prior events.
    for call_id in ["run-2:call-1:0", "run-2:call-1", "run-2:call-1:17"] {
        let mut state =
            WorkbenchState::new(DemoSource(Vec::new()), &UiSettings::default()).unwrap();
        state.apply_events([Event::new(ToolEvent::ApprovalRequested {
            tool_name: "shell".into(),
            call_id: call_id.into(),
        })]);
        assert_eq!(
            state
                .notifications()
                .items()
                .next()
                .unwrap()
                .run_id
                .as_deref(),
            Some("run-2")
        );
        let path = state
            .dock()
            .find_tab(&PanelId::new("notifications-main"))
            .unwrap();
        state.dock_mut().set_active_tab(path).unwrap();
        let mut harness = Harness::builder()
            .with_size(egui::vec2(1280.0, 900.0))
            .build_ui_state(
                |ui, state| state.ui(ui, &mut eframe::Frame::_new_kittest()),
                state,
            );
        harness.run_steps(3);
        let panel = PanelId::new("agent-run-2");
        assert!(harness.state().dock().find_tab(&panel).is_none());
        // When: the notification row is clicked once.
        harness.get_by_label("Approval requested: shell").click();
        harness.run_steps(3);
        // Then: the owning run's transcript is the active tab.
        let dock = harness.state().dock();
        let path = dock.find_tab(&panel).expect("run transcript opened");
        assert_eq!(dock.leaf(path.node_path()).unwrap().active, path.tab);
    }
}

#[test]
fn scoped_prefix_wins_over_conflicting_call_index() {
    // Given: an index entry conflicting with an explicit scoped ID.
    let mut registry = TranscriptRegistry::new();
    registry.apply(&Event::new(ToolEvent::ToolStarted {
        tool_name: "shell".into(),
        call_id: "run-2:call-1:0".into(),
        run_id: Some("run-9".into()),
        input: None,
    }));
    // When/Then: explicit run scope wins without changing index registration.
    assert_eq!(registry.run_for_call("run-2:call-1:0"), Some("run-2"));
}

#[test]
fn malformed_scoped_ids_use_only_exact_call_index() {
    // Given: IDs that do not have an ASCII run-number prefix followed by a colon.
    for call_id in [
        "run-:call",
        "run-x:call",
        "run-2x:call",
        "run-+2:call",
        "run-２:call",
        "other:call",
        "run-2",
        ":run-2:call",
    ] {
        let mut registry = TranscriptRegistry::new();
        assert_eq!(registry.run_for_call(call_id), None);
        registry.apply(&Event::new(ToolEvent::ToolStarted {
            tool_name: "shell".into(),
            call_id: call_id.into(),
            run_id: Some("run-9".into()),
            input: None,
        }));
        // When/Then: a full matching index entry, not a guessed prefix, resolves the ID.
        assert_eq!(registry.run_for_call(call_id), Some("run-9"));
    }
}
