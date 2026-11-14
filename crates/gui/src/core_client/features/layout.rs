//! How the feature screens lay out lists of unbounded or large size.
//!
//! The feature screens are stacked in one scrolling column, so a list that grows
//! without bound would push everything below it, and the control that pages it,
//! out of reach. A list therefore scrolls inside an area of bounded height, and the
//! control that pages it stays below that area, in view.

/// The height at which a list starts to scroll.
pub(super) const LIST_MAX_HEIGHT: f32 = 240.0;

/// The rows of a list in an area that scrolls once it is [`LIST_MAX_HEIGHT`] tall.
pub(super) fn bounded_list<R>(
    ui: &mut egui::Ui,
    id: &'static str,
    rows: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    egui::ScrollArea::vertical()
        .id_salt(id)
        .max_height(LIST_MAX_HEIGHT)
        .show(ui, rows)
        .inner
}

/// A list that is read page by page: its rows scroll in a bounded area and the button that
/// asks for the next page, shown while `next_page` names it, sits below the area. Returns
/// whether that button was clicked.
pub(super) fn paged_list(
    ui: &mut egui::Ui,
    id: &'static str,
    next_page: Option<&str>,
    rows: impl FnOnce(&mut egui::Ui),
) -> bool {
    bounded_list(ui, id, rows);
    next_page.is_some_and(|label| ui.button(label).clicked())
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui_kittest::{Harness, kittest::Queryable};

    #[test]
    fn a_long_list_takes_no_more_than_its_bound_and_what_follows_stays_in_view() {
        let mut harness = Harness::builder()
            .with_size(egui::vec2(800.0, 600.0))
            .build_ui(|ui| {
                bounded_list(ui, "rows", |ui| {
                    for n in 0..500 {
                        ui.label(format!("row {n}"));
                    }
                });
                ui.label("after the list");
            });
        harness.run();
        let after = harness.get_by_label("after the list").rect();
        assert!(
            after.max.y <= LIST_MAX_HEIGHT + 40.0,
            "the control below a list sits within the window: {after:?}"
        );
    }
}
