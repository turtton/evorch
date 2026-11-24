//! How the feature screens lay out lists of unbounded or large size.
//!
//! A feature screen may be stacked with others in one scrolling column, or fill a tab of
//! the docked desktop. Either way a list that grows without bound would push everything
//! below it, and the control that pages it, out of reach. A list therefore scrolls inside
//! an area of bounded height, and the control that pages it stays below that area, in view.
//! In a tab the bound grows with the tab, so a tall tab is not left mostly empty.

/// The height at which a list starts to scroll in the stacked column, and at least in a tab.
pub(super) const LIST_MAX_HEIGHT: f32 = 240.0;

/// The share of a tab's height a list may take, leaving room for what follows it.
const TAB_SHARE: f32 = 0.55;

/// The share for the list of a tab that shows little else: a line above it, and its
/// pager and a folded line below.
const MAIN_LIST_TAB_SHARE: f32 = 0.75;

#[derive(Clone, Copy)]
struct TabHeight(f32);

fn tab_height_id() -> egui::Id {
    egui::Id::new("core-client-feature-tab-height")
}

/// Draw a feature body as the content of a dock tab: it scrolls as a whole, and its lists
/// take up to about half of the tab.
pub(super) fn in_tab<R>(ui: &mut egui::Ui, body: impl FnOnce(&mut egui::Ui) -> R) -> R {
    let height = ui.available_height();
    let previous = ui.ctx().data(|d| d.get_temp::<TabHeight>(tab_height_id()));
    ui.ctx()
        .data_mut(|d| d.insert_temp(tab_height_id(), TabHeight(height)));
    let inner = egui::ScrollArea::vertical()
        .id_salt("core-feature-tab")
        .auto_shrink([false, false])
        .show(ui, body)
        .inner;
    ui.ctx().data_mut(|d| {
        if let Some(previous) = previous {
            d.insert_temp(tab_height_id(), previous);
        } else {
            d.remove::<TabHeight>(tab_height_id());
        }
    });
    inner
}

fn list_max_height(ui: &egui::Ui, share: f32) -> f32 {
    ui.ctx()
        .data(|d| d.get_temp::<TabHeight>(tab_height_id()))
        .map_or(LIST_MAX_HEIGHT, |TabHeight(tab)| {
            (tab * share).max(LIST_MAX_HEIGHT)
        })
}

/// The rows of a list in an area that scrolls once it is [`LIST_MAX_HEIGHT`] tall.
pub(super) fn bounded_list<R>(
    ui: &mut egui::Ui,
    id: &'static str,
    rows: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    bounded_list_sharing(ui, id, TAB_SHARE, rows)
}

fn bounded_list_sharing<R>(
    ui: &mut egui::Ui,
    id: &'static str,
    share: f32,
    rows: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    egui::ScrollArea::vertical()
        .id_salt(id)
        .max_height(list_max_height(ui, share))
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

/// [`paged_list`] for the list a tab is about: in a tab it may take most of the
/// height, and in the stacked column it is bounded as any other list.
pub(super) fn main_paged_list(
    ui: &mut egui::Ui,
    id: &'static str,
    next_page: Option<&str>,
    rows: impl FnOnce(&mut egui::Ui),
) -> bool {
    bounded_list_sharing(ui, id, MAIN_LIST_TAB_SHARE, rows);
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
