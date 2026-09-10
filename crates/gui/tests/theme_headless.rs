use egui::accesskit::Role;
use egui::{Theme, vec2};
use egui_kittest::{
    Harness,
    kittest::{By, Queryable},
};
use gui::theme::tokens::{ACCENT, DOT_SIZE, ROW_DENSE};
use gui::theme::widgets::{compact_row, status_dot};

#[test]
fn compact_row_is_dense_and_centers_status_dot() {
    // Given: a themed compact row containing a status dot and a single-line title
    let row_rect = std::cell::Cell::new(egui::Rect::ZERO);
    let dot_rect = std::cell::Cell::new(egui::Rect::ZERO);
    let title_rect = std::cell::Cell::new(egui::Rect::ZERO);
    let mut harness = Harness::builder()
        .with_size(vec2(400.0, 300.0))
        .build_ui(|ui| {
            gui::theme::install(ui.ctx());
            row_rect.set(
                compact_row(ui, false, |ui| {
                    dot_rect.set(status_dot(ui, ACCENT).rect);
                    title_rect.set(
                        ui.add_sized(
                            vec2(ui.available_width(), ROW_DENSE),
                            egui::Label::new("evorch")
                                .truncate()
                                .sense(egui::Sense::click()),
                        )
                        .rect,
                    );
                })
                .rect,
            );
        });

    // When: the layout settles
    harness.run();

    // Then: the dense row centers both widgets without overlap
    let row = row_rect.get();
    let dot = dot_rect.get();
    let title = title_rect.get();
    assert!(
        (row.height() - ROW_DENSE).abs() <= 0.5,
        "row height {} should be {ROW_DENSE} +/- 0.5; row={row:?}, dot={dot:?}, title={title:?}",
        row.height()
    );
    assert_eq!(dot.size(), vec2(DOT_SIZE, DOT_SIZE));
    assert!((dot.center().y - row.center().y).abs() <= 0.5);
    assert!((title.center().y - row.center().y).abs() <= 0.5);
    assert!(dot.right() <= title.left());
}

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

#[derive(Clone)]
struct MockSource(Vec<runtime::AgentSummary>);

impl gui::model::tasks::AgentRunSource for MockSource {
    fn list(&self) -> Vec<runtime::AgentSummary> {
        self.0.clone()
    }
}

fn build_harness(
    state: gui::app::WorkbenchState<MockSource>,
) -> Harness<'static, gui::app::WorkbenchState<MockSource>> {
    Harness::builder()
        .with_size(vec2(800.0, 600.0))
        .build_ui_state(
            |ui, state: &mut gui::app::WorkbenchState<MockSource>| {
                state.ui(ui, &mut eframe::Frame::_new_kittest());
            },
            state,
        )
}

#[test]
fn workbench_installs_theme_on_first_frame() {
    // Given: a default workbench state
    let state =
        gui::app::WorkbenchState::new(MockSource(Vec::new()), &workspace_ui::UiSettings::default())
            .expect("default state builds");
    let mut harness = build_harness(state);

    // When: the UI is rendered
    harness.step();

    // Then: the dark design tokens are installed.
    assert_eq!(
        harness.ctx.style_of(Theme::Dark).visuals.panel_fill,
        gui::theme::tokens::CANVAS
    );
    assert_eq!(harness.ctx.theme(), Theme::Dark);
}

#[test]
fn dock_style_distinguishes_tab_states() {
    // Given: the dock style derived from the theme style
    let style = gui::theme::style::style();
    let dock = gui::theme::dock::dock_style(&style);

    // Then: tab states are visually distinct and sized as specified.
    assert_ne!(dock.tab.active.bg_fill, dock.tab.inactive.bg_fill);
    assert_ne!(dock.tab.hovered.text_color, dock.tab.inactive.text_color);
    assert_eq!(dock.tab.active.outline_color, gui::theme::tokens::ACCENT);
    assert_eq!(dock.tab_bar.height, 28.0);
    assert_eq!(dock.tab.tab_body.inner_margin, egui::Margin::same(8));
}

#[test]
fn attention_tab_style_overrides_text_and_outline() {
    // Given: a base dock tab style and an attention color
    let base = gui::theme::dock::dock_style(&gui::theme::style::style()).tab;
    let color = gui::theme::tokens::WARNING_FG;
    let attention = gui::theme::dock::attention_tab_style(&base, color);

    // Then: the attention color is applied to text and outline.
    assert_eq!(attention.inactive.text_color, color);
    assert_eq!(attention.active.outline_color, color);
    assert_eq!(attention.hovered.text_color, color);
}

#[test]
fn demo_state_has_no_attention_for_removed_merge_tab() {
    // Given: a populated demo workbench with a bound, unresolved PR
    let dir = tempfile::tempdir().expect("temp dir");
    let sidebar = gui::fixture::demo_sidebar(dir.path()).expect("demo sidebar");
    let state = gui::fixture::populate(
        gui::app::WorkbenchState::new(
            gui::fixture::DemoSource(gui::fixture::demo_runs()),
            &workspace_ui::UiSettings::default(),
        )
        .expect("default state builds"),
        sidebar,
    );

    // Then: the merge tab carries the warning attention accent.
    assert_eq!(
        state.pane_attention(&workspace_ui::PanelId::new("merge-main")),
        None
    );
}
