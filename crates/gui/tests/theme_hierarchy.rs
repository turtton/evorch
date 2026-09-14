use std::cell::Cell;

use egui::{Color32, Key, Rect, Stroke, epaint::Shape};
use egui_kittest::{Harness, kittest::Queryable};
use gui::theme::{
    dock::dock_style,
    style::{ThemePreset, style_for},
    tokens::palette,
    widgets,
};

#[test]
fn widget_states_use_palette_roles_when_each_preset_is_built() {
    // Given: every supported preset without mutating the global palette.
    for preset in [
        ThemePreset::Graphite,
        ThemePreset::HighContrast,
        ThemePreset::TokyoNight,
    ] {
        let p = preset.palette();
        // When: the style is built.
        let v = style_for(preset).visuals;
        // Then: elevation and interaction roles are explicit and borders are hairlines.
        assert_eq!(
            [
                v.panel_fill,
                v.faint_bg_color,
                v.code_bg_color,
                v.window_fill
            ],
            [p.CANVAS, p.SURFACE, p.SURFACE_RAISED, p.OVERLAY]
        );
        for (state, fill, border) in [
            (v.widgets.inactive, p.SURFACE_RAISED, p.BORDER),
            (v.widgets.hovered, p.HOVER_ROW, p.BORDER),
            (v.widgets.active, p.ACTIVE_ROW, p.ACCENT),
            (v.widgets.open, p.ACTIVE_ROW, p.ACCENT),
        ] {
            assert_eq!(state.bg_fill, fill, "{preset:?}");
            assert_eq!(state.weak_bg_fill, fill);
            assert_eq!(state.bg_stroke, Stroke::new(1.0, border));
            assert_eq!(state.fg_stroke.width, 1.0);
        }
        assert_ne!(p.HOVER_ROW, p.ACTIVE_ROW);
    }
}

fn painted_rect(
    shapes: &[egui::epaint::ClippedShape],
    rect: Rect,
    paint: (Color32, Stroke),
) -> bool {
    shapes.iter().any(|shape| {
        matches!(&shape.shape, Shape::Rect(painted)
        if painted.rect == rect && (painted.fill, painted.stroke) == paint)
    })
}

#[test]
fn button_paints_subtle_hover_when_pointer_enters() {
    // Given: a native button using the workbench style.
    let rect = Cell::new(Rect::ZERO);
    let mut harness = Harness::new_ui(|ui| {
        ui.set_style(style_for(ThemePreset::Graphite));
        rect.set(ui.button("Action").rect);
    });
    // When: a real pointer hover is delivered.
    harness.get_by_label("Action").hover();
    harness.run();
    // Then: the painted button uses a subtle palette fill and border.
    let p = palette();
    assert!(painted_rect(
        &harness.output().shapes,
        rect.get(),
        (p.HOVER_ROW, Stroke::new(1.0, p.BORDER))
    ));
}

#[test]
fn button_paints_accent_ring_when_tab_focused() {
    // Given: a native button using the workbench style.
    let rect = Cell::new(Rect::ZERO);
    let focused = Cell::new(false);
    let mut harness = Harness::new_ui(|ui| {
        ui.set_style(style_for(ThemePreset::Graphite));
        let response = ui.button("Action");
        rect.set(response.rect);
        focused.set(response.has_focus());
    });
    // When: keyboard traversal focuses the button.
    harness.key_press(Key::Tab);
    harness.run();
    // Then: native focus is retained and visibly painted, not just stored in style.
    let p = palette();
    assert!(focused.get());
    assert!(painted_rect(
        &harness.output().shapes,
        rect.get(),
        (p.ACTIVE_ROW, Stroke::new(1.0, p.ACCENT))
    ));
}

#[test]
fn selected_row_paints_stronger_fill_when_selected() {
    // Given: a selected compact row.
    let rect = Cell::new(Rect::ZERO);
    let mut harness = Harness::new_ui(|ui| {
        rect.set(
            widgets::compact_row(ui, true, |ui| {
                ui.label("Selected");
            })
            .rect,
        );
    });
    // When: the row is rendered.
    harness.run();
    // Then: selection is stronger than hover even in Graphite.
    assert!(painted_rect(
        &harness.output().shapes,
        rect.get(),
        (palette().ACTIVE_ROW, Stroke::NONE)
    ));
}

#[test]
fn dock_chrome_uses_raised_elevation_when_built() {
    // Given: the installed palette and workbench style.
    let p = palette();
    // When: dock chrome is derived.
    let dock = dock_style(&style_for(ThemePreset::Graphite));
    // Then: permanent chrome stays below overlays and selection is explicit.
    assert_eq!(dock.tab_bar.bg_fill, p.SURFACE_RAISED);
    assert_eq!(dock.tab.inactive.bg_fill, p.SURFACE_RAISED);
    assert_eq!(dock.tab.active.bg_fill, p.ACTIVE_ROW);
    assert_eq!(dock.tab.focused.bg_fill, p.ACTIVE_ROW);
    assert_eq!(dock.separator.width, 1.0);
    assert_eq!(dock.tab.tab_body.stroke, Stroke::NONE);
    assert_eq!(dock.main_surface_border_stroke, Stroke::NONE);
    for state in [
        dock.tab.inactive_with_kb_focus,
        dock.tab.active_with_kb_focus,
        dock.tab.focused_with_kb_focus,
    ] {
        assert_eq!(state.outline_color, p.ACCENT);
    }
    assert_eq!(
        widgets::surface_frame(p.SURFACE).stroke,
        Stroke::new(1.0, p.BORDER)
    );
}
