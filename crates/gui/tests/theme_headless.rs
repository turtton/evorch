use egui::accesskit::Role;
use egui::{Theme, vec2};
use egui_kittest::{
    Harness,
    kittest::{By, Queryable},
};

#[test]
fn pane_root_exposes_title_as_accessible_pane_landmark() {
    // Given: a pane_root wrapper with title "Projects" and body label "body"
    let mut harness = Harness::builder()
        .with_size(vec2(400.0, 300.0))
        .build_ui(|ui| {
            gui::theme::widgets::pane_root(ui, "Projects", |ui| {
                ui.label("body");
            });
        });
    harness.run();

    // Then: the title is reachable as a single Pane landmark.
    harness.get_by_label("Projects");
    assert_eq!(harness.query_all_by_label("Projects").count(), 1);
    assert!(
        harness
            .query(By::new().role(Role::Pane).label("Projects"))
            .is_some()
    );
}

#[test]
fn install_applies_dark_design_tokens() {
    // Given: a fresh harness
    let mut harness = Harness::builder()
        .with_size(vec2(400.0, 300.0))
        .build_ui(|ui| {
            gui::theme::install(ui.ctx());
        });
    harness.run();

    // Then: dark theme tokens are installed.
    let style = harness.ctx.style_of(Theme::Dark);
    assert_eq!(style.visuals.panel_fill, gui::theme::tokens::CANVAS);
    assert!(style.visuals.dark_mode);
    assert_eq!(style.text_styles[&egui::TextStyle::Body].size, 14.0);
    assert_eq!(style.spacing.item_spacing, egui::vec2(8.0, 4.0));
}

#[test]
fn empty_state_cta_click_is_reported() {
    // Given: an empty state with a CTA button
    let clicked = std::cell::Cell::new(false);
    let mut harness = Harness::builder()
        .with_size(vec2(400.0, 300.0))
        .build_ui(|ui| {
            if gui::theme::widgets::empty_state(ui, "No project", "hint", Some("Go to Projects")) {
                clicked.set(true);
            }
        });
    harness.run();

    // When: the CTA is clicked.
    harness.get_by_label("Go to Projects").click();
    harness.run();

    // Then: the closure reports the click.
    assert!(clicked.get());
}
