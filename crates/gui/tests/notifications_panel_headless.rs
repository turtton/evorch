use egui::{Color32, epaint::Shape};
use egui_kittest::{Harness, kittest::Queryable};
use event_bus::{AgentRunPhase, Event, LifecycleEvent};
use gui::{app::WorkbenchState, model::notifications::NotificationsModel, theme::tokens::SUCCESS};
use workspace_ui::PanelId;

fn done() -> Event {
    Event::new(LifecycleEvent::AgentRunStateChanged {
        run_id: "notification-run".into(),
        from: AgentRunPhase::Running,
        to: AgentRunPhase::Done,
        reason: None,
    })
}

fn panel_harness(active: &str) -> Harness<'static, WorkbenchState<gui::fixture::DemoSource>> {
    let mut state = WorkbenchState::new(
        gui::fixture::DemoSource(Vec::new()),
        &workspace_ui::UiSettings::default(),
    )
    .expect("workbench");
    state.notifications_mut().apply_event(&done(), |_| None);
    let path = state.dock().find_tab(&PanelId::new(active)).expect("tab");
    state.dock_mut().set_active_tab(path).expect("activate");
    Harness::builder()
        .with_size(egui::vec2(1280.0, 720.0))
        .build_ui_state(
            |ui, state| state.ui(ui, &mut eframe::Frame::_new_kittest()),
            state,
        )
}

#[test]
fn unread_notification_badge_filled_read_outline() {
    // Given: an unread completed notification.
    let mut model = NotificationsModel::default();
    model.apply_event(&done(), |_| None);
    let mut harness = Harness::builder().build_ui_state(
        |ui, model| {
            gui::panes::notifications::notifications_pane(ui, model, None);
        },
        model,
    );
    // When: rendering before and after acknowledging the displayed revision.
    harness.run();
    assert!(harness.output().shapes.iter().any(|shape| {
        matches!(&shape.shape, Shape::Rect(rect) if rect.fill == SUCCESS
            && rect.rect.height() < gui::theme::tokens::ROW_DENSE)
    }));
    let id = harness.state().items().next().expect("notification").id;
    let revision = harness.state().revision(id).expect("revision");
    harness
        .state_mut()
        .acknowledge(id, Some(&revision), Some(true));
    harness.run_steps(2);
    // Then: the same badge is outline-only.
    assert!(harness.output().shapes.iter().any(|shape| {
        matches!(&shape.shape, Shape::Rect(rect) if rect.fill == Color32::TRANSPARENT
            && rect.stroke.color == SUCCESS && rect.stroke.width > 0.0)
    }));
    assert!(
        !harness
            .output()
            .shapes
            .iter()
            .any(|shape| { matches!(&shape.shape, Shape::Rect(rect) if rect.fill == SUCCESS) })
    );
}

#[test]
fn panel_ack_clears_unread_when_displayed_and_focused() {
    // Given: the active Notifications panel.
    let mut harness = panel_harness("notifications-main");
    // When: displayed in an explicitly focused viewport.
    harness
        .input_mut()
        .viewports
        .entry(egui::ViewportId::ROOT)
        .or_default()
        .focused = Some(true);
    harness.run_steps(3);
    // Then: its notification is acknowledged.
    assert_eq!(harness.state().notifications().unread_count(), 0);
    harness
        .get_by_label("Run notification-run completed")
        .click();
    harness.run_steps(3);
    assert!(
        harness
            .state()
            .dock()
            .find_tab(&PanelId::new("agent-notification-run"))
            .is_some()
    );
}

#[test]
fn panel_stays_unread_without_outer_focus() {
    for focused in [None, Some(false)] {
        // Given: the active Notifications panel.
        let mut harness = panel_harness("notifications-main");
        // When: displayed without explicit outer focus.
        harness
            .input_mut()
            .viewports
            .entry(egui::ViewportId::ROOT)
            .or_default()
            .focused = focused;
        harness.run_steps(3);
        // Then: it remains unread.
        assert_eq!(harness.state().notifications().unread_count(), 1);
    }
}

#[test]
fn hidden_notifications_tab_stays_unread() {
    // Given: Notifications hidden behind Agents in the same leaf.
    let mut harness = panel_harness("agents-main");
    // When: only the active tab is displayed in a focused viewport.
    harness
        .input_mut()
        .viewports
        .entry(egui::ViewportId::ROOT)
        .or_default()
        .focused = Some(true);
    harness.run_steps(3);
    // Then: the hidden notification remains unread.
    assert_eq!(harness.state().notifications().unread_count(), 1);
}
