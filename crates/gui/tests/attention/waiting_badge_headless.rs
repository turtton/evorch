use egui::{Color32, epaint::Shape};
use egui_kittest::Harness;
use gui::{app::WorkbenchState, theme::tokens::palette};
use workspace_ui::ThreadRunPhase;

type Workbench = WorkbenchState<runtime::AgentRuntime>;

fn waiting_transcript() -> WorkbenchState<gui::fixture::DemoSource> {
    let mut state = WorkbenchState::new(
        gui::fixture::DemoSource(Vec::new()),
        &workspace_ui::UiSettings::default(),
    )
    .expect("workbench");
    state.add_project(std::env::current_dir().unwrap()).unwrap();
    state.create_thread("waiting transcript").unwrap();
    let mut sidebar = state.sidebar().clone();
    sidebar.threads[0].run_ids = vec!["waiting-run".into()];
    let mut state = state.with_sidebar(sidebar);
    state.open_agent_pane("waiting-run");
    state.apply_events([event_bus::Event::new(
        event_bus::LifecycleEvent::AgentRunStateChanged {
            run_id: "waiting-run".into(),
            from: event_bus::AgentRunPhase::Running,
            to: event_bus::AgentRunPhase::Waiting,
            reason: None,
        },
    )]);
    state
}

#[test]
fn unread_waiting_badge_filled_info_accent() {
    // Given: a waiting revision that has not been displayed.
    let mut ack = Workbench::new_attention_ack();
    ack.observe(ThreadRunPhase::Waiting).expect("observe");
    // When: the real badge is rendered.
    let mut harness = Harness::new_ui(|ui| {
        gui::panes::phase_indicator::phase_indicator_with_ack(ui, ack.phase(), ack.is_unread());
    });
    harness.run();
    // Then: waiting is a filled info-blue pill, not warning yellow.
    assert!(
        harness
            .output()
            .shapes
            .iter()
.any(|shape| { matches!(&shape.shape, Shape::Rect(rect) if rect.fill == palette().INFO && rect.rect.height() < gui::theme::tokens::ROW_DENSE) })
    );
}

#[test]
fn read_waiting_badge_outline_only() {
    // Given: the current revision was displayed in a focused outer window.
    let mut ack = Workbench::new_attention_ack();
    ack.observe(ThreadRunPhase::Waiting).expect("observe");
    let displayed = ack.revision();
    ack.acknowledge_surface(Some(&displayed), Some(true));
    // When: the acknowledged badge is rendered again.
    let mut harness = Harness::new_ui(|ui| {
        gui::panes::phase_indicator::phase_indicator_with_ack(ui, ack.phase(), ack.is_unread());
    });
    harness.run();
    // Then: the pill has no fill and retains its info-blue outline.
    assert!(harness.output().shapes.iter().any(|shape| {
        matches!(&shape.shape, Shape::Rect(rect)
if rect.fill == Color32::TRANSPARENT && rect.stroke.color == palette().INFO && rect.stroke.width > 0.0)
    }));
}

#[test]
fn transcript_tab_quiet_after_ack() {
    // Given: a waiting run's transcript is selected in its owning thread.
    let state = waiting_transcript();
    let mut harness = Harness::builder()
        .with_size(egui::vec2(1280.0, 720.0))
        .build_ui_state(
            |ui, state| {
                state.ui(ui, &mut eframe::Frame::_new_kittest());
            },
            state,
        );
    // When: the pane is actually displayed with explicit outer focus.
    harness
        .input_mut()
        .viewports
        .entry(egui::ViewportId::ROOT)
        .or_default()
        .focused = Some(true);
    harness.run_steps(3);
    // Then: acknowledged tabs no longer advertise attention.
    assert_eq!(
        harness
            .state()
            .pane_attention(&workspace_ui::PanelId::new("agent-waiting-run")),
        None
    );
}

#[test]
fn visible_transcript_stays_unread_without_outer_focus() {
    // Given: a waiting run in a visible pane, without explicit focus.
    for focused in [None, Some(false)] {
        let state = waiting_transcript();
        let mut harness = Harness::builder()
            .with_size(egui::vec2(1280.0, 720.0))
            .build_ui_state(
                |ui, state| {
                    state.ui(ui, &mut eframe::Frame::_new_kittest());
                },
                state,
            );
        // When: frames are rendered without an outer focus acknowledgement.
        harness
            .input_mut()
            .viewports
            .entry(egui::ViewportId::ROOT)
            .or_default()
            .focused = focused;
        harness.run_steps(3);
        // Then: the waiting tab retains its info accent.
        assert_eq!(
            harness
                .state()
                .pane_attention(&workspace_ui::PanelId::new("agent-waiting-run")),
            Some(palette().INFO)
        );
    }
}

#[test]
#[ignore = "writes unread/read waiting PNG evidence using an offscreen adapter"]
fn capture_waiting_read_unread_png_evidence() {
    // Given: the same waiting badge before and after proper acknowledgement.
    let output = std::env::var_os("EVORCH_WAITING_EVIDENCE")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp/opencode"));
    std::fs::create_dir_all(&output).expect("evidence directory");
    for read in [false, true] {
        let mut ack = Workbench::new_attention_ack();
        ack.observe(ThreadRunPhase::Waiting).expect("observe");
        if read {
            ack.acknowledge_surface(Some(&ack.revision()), Some(true));
        }
        let mut harness = Harness::builder()
            .with_size(egui::vec2(320.0, 100.0))
            .build_ui(|ui| {
                gui::theme::install(ui.ctx());
                gui::panes::phase_indicator::phase_indicator_with_ack(
                    ui,
                    ack.phase(),
                    ack.is_unread(),
                );
            });
        harness.run();
        // When: rendering the real egui surface offscreen.
        let image = harness.render().expect("offscreen adapter");
        // Then: save both states for visual review.
        image
            .save(output.join(if read {
                "waiting-read.png"
            } else {
                "waiting-unread.png"
            }))
            .expect("PNG");
    }
}

#[test]
fn hidden_transcript_stays_unread_in_focused_window() {
    // Given: a waiting transcript hidden behind Conversation in the same leaf.
    let mut state = waiting_transcript();
    let path = state
        .dock()
        .find_tab(&workspace_ui::PanelId::new("agent-main"))
        .expect("conversation tab");
    state.dock_mut().set_active_tab(path).expect("activate");
    let mut harness = Harness::builder()
        .with_size(egui::vec2(1280.0, 720.0))
        .build_ui_state(
            |ui, state| {
                state.ui(ui, &mut eframe::Frame::_new_kittest());
            },
            state,
        );
    // When: a focused frame paints only the active tab.
    harness
        .input_mut()
        .viewports
        .entry(egui::ViewportId::ROOT)
        .or_default()
        .focused = Some(true);
    harness.run_steps(3);
    // Then: an unrendered surface cannot be acknowledged.
    assert_eq!(
        harness
            .state()
            .pane_attention(&workspace_ui::PanelId::new("agent-waiting-run")),
        Some(palette().INFO)
    );
}
