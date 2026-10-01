use std::sync::Arc;

use egui::{ViewportCommand, ViewportEvent, ViewportId};
use egui_kittest::{Harness, kittest::Queryable};
use gui::{app::WorkbenchState, fixture::DemoSource};
use runtime::ownership::OwnerHost;
use workspace_ui::UiSettings;

#[test]
fn app_close_is_not_cancelled_when_echoed_by_the_backend() {
    // Given: an idle owner and a real workbench frame.
    let dir = tempfile::tempdir().unwrap();
    let host = Arc::new(
        OwnerHost::open(
            dir.path(),
            Default::default(),
            Arc::new(event_bus::EventBus::new(64)),
        )
        .unwrap(),
    );
    let permit = host.start("thread").unwrap();
    let state = WorkbenchState::new(DemoSource(Vec::new()), &UiSettings::default())
        .unwrap()
        .with_ownership(host.clone());
    let mut harness = Harness::builder().build_ui_state(
        |ui, state: &mut WorkbenchState<DemoSource>| {
            state.ui(ui, &mut eframe::Frame::_new_kittest());
        },
        state,
    );
    harness
        .input_mut()
        .viewports
        .get_mut(&ViewportId::ROOT)
        .unwrap()
        .events
        .push(ViewportEvent::Close);
    harness.step();
    let commands = &harness.output().viewport_output[&ViewportId::ROOT].commands;
    assert!(commands.contains(&ViewportCommand::CancelClose));
    assert!(commands.contains(&ViewportCommand::Close));
    assert!(harness.query_by_label("Keep open").is_none());
    assert!(harness.query_by_label("Active thread ownership").is_none());

    // The viewport close only requests quiescence. Durable queued mutations
    // remain valid until the production storage owner finishes draining.
    assert_eq!(
        host.attach("thread").unwrap().state,
        runtime::ownership::OwnerState::Quiescing
    );
    permit.validate_generation().unwrap();
    assert!(permit.begin_turn().is_err());

    // When: the backend echoes the application's own close command.
    harness
        .input_mut()
        .viewports
        .get_mut(&ViewportId::ROOT)
        .unwrap()
        .events
        .push(ViewportEvent::Close);
    harness.step();

    // Then: the close is allowed through instead of being self-cancelled.
    assert!(
        !harness.output().viewport_output[&ViewportId::ROOT]
            .commands
            .contains(&ViewportCommand::CancelClose)
    );
}
