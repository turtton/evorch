use egui::{Color32, epaint::Shape};
use egui_kittest::Harness;
use gui::{app::WorkbenchState, theme::tokens::INFO};
use workspace_ui::ThreadRunPhase;

type Workbench = WorkbenchState<runtime::AgentRuntime>;

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
            .any(|shape| { matches!(&shape.shape, Shape::Rect(rect) if rect.fill == INFO && rect.rect.height() < gui::theme::tokens::ROW_DENSE) })
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
            if rect.fill == Color32::TRANSPARENT && rect.stroke.color == INFO && rect.stroke.width > 0.0)
    }));
}

#[test]
fn dock_tab_quiet_after_ack() {
    // Given: a waiting agent in the visible Agents pane.
    let mut runs = gui::fixture::demo_runs();
    for run in &mut runs {
        run.phase = event_bus::AgentRunPhase::Waiting;
    }
    let state = WorkbenchState::new(
        gui::fixture::DemoSource(runs),
        &workspace_ui::UiSettings::default(),
    )
    .expect("workbench");
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
            .pane_attention(&workspace_ui::PanelId::new("agents-main")),
        None
    );
}

#[test]
fn visible_tab_stays_unread_without_outer_focus() {
    // Given: a waiting run in a visible pane, without explicit focus.
    for focused in [None, Some(false)] {
        let mut runs = gui::fixture::demo_runs();
        for run in &mut runs {
            run.phase = event_bus::AgentRunPhase::Waiting;
        }
        let state = WorkbenchState::new(
            gui::fixture::DemoSource(runs),
            &workspace_ui::UiSettings::default(),
        )
        .expect("state");
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
                .pane_attention(&workspace_ui::PanelId::new("agents-main")),
            Some(INFO)
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
fn hidden_tab_stays_unread_in_focused_window() {
    // Given: waiting Agents hidden behind the Diff tab.
    let mut runs = gui::fixture::demo_runs();
    for run in &mut runs {
        run.phase = event_bus::AgentRunPhase::Waiting;
    }
    let mut state = WorkbenchState::new(
        gui::fixture::DemoSource(runs),
        &workspace_ui::UiSettings::default(),
    )
    .expect("state");
    let path = state
        .dock()
        .find_tab(&workspace_ui::PanelId::new("diff-main"))
        .expect("diff tab");
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
            .pane_attention(&workspace_ui::PanelId::new("agents-main")),
        Some(INFO)
    );
}
