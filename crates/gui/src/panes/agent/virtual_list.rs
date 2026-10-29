//! Measured, variable-height transcript rows. Unknown rows are never estimated:
//! the first pass (and a width change) lays out every row once.

use std::collections::{HashMap, HashSet};

#[derive(Clone, Copy)]
struct MeasuredRow {
    revision: u64,
    height: f32,
}

#[derive(Clone, Default)]
pub(super) struct VirtualList {
    heights: HashMap<usize, MeasuredRow>,
    width: f32,
    enabled: bool,
    scope_id: Option<egui::Id>,
}

fn override_id() -> egui::Id {
    egui::Id::new("transcript-virtualization-test-override")
}

/// Internal test hook: kittest enables AccessKit, which normally requires the
/// complete transcript. Set this before drawing to exercise the production
/// virtualized path while retaining kittest's queryable labels.
#[doc(hidden)]
pub fn set_transcript_virtualization_for_tests(ctx: &egui::Context, enabled: bool) {
    ctx.data_mut(|data| data.insert_temp(override_id(), enabled));
}

/// Content extent and scroll offset of the last transcript drawn with the test
/// override. This observes layout even when the changed row is offscreen.
#[doc(hidden)]
pub fn transcript_layout_for_tests(ctx: &egui::Context) -> Option<(egui::Vec2, egui::Vec2)> {
    ctx.data(|data| data.get_temp(override_id().with("layout")))
}

pub(super) fn record_layout(ui: &egui::Ui, content: egui::Vec2, offset: egui::Vec2) {
    ui.data_mut(|data| {
        if data.get_temp::<bool>(override_id()).is_some() {
            data.insert_temp(override_id().with("layout"), (content, offset));
        }
    });
}

impl VirtualList {
    pub(super) fn load(ui: &egui::Ui, id: egui::Id, entries: impl Iterator<Item = usize>) -> Self {
        let enabled = ui
            .data(|data| data.get_temp::<bool>(override_id()))
            .unwrap_or_else(|| ui.ctx().accesskit_node_builder(ui.id(), |_| ()).is_none());
        // Move rather than clone the map on every frame, and never hold the
        // context lock while rendering widgets.
        let mut cache = ui
            .data_mut(|data| data.remove_temp::<Self>(id))
            .unwrap_or_default();
        let width = ui.available_width();
        if cache.width != width {
            cache.heights.clear();
            cache.width = width;
        }
        cache.enabled = enabled;
        cache.scope_id = Some(id);
        let retained: HashSet<_> = entries.collect();
        cache.heights.retain(|entry, _| retained.contains(entry));
        cache
    }

    pub(super) fn store(self, ui: &egui::Ui, id: egui::Id) {
        ui.data_mut(|data| data.insert_temp(id, self));
    }

    pub(super) fn row(
        &mut self,
        ui: &mut egui::Ui,
        viewport: egui::Rect,
        entry_id: usize,
        revision: u64,
        draw: impl FnOnce(&mut egui::Ui),
    ) {
        let height = self
            .heights
            .get(&entry_id)
            .filter(|row| row.revision == revision)
            .map(|row| row.height);
        let top = ui.cursor().top();
        let skip = self.enabled
            && height
                .is_some_and(|height| top + height < viewport.top() || top > viewport.bottom());
        // An explicit ID also severs egui's parent auto-ID counter dependency:
        // eviction or consolidation of earlier rows must not lose selection.
        let scope = self.scope_id.expect("list loaded").with(entry_id);
        let response = ui.scope_builder(egui::UiBuilder::new().id(scope), |ui| {
            if skip {
                ui.add_space(height.unwrap_or_default());
            } else {
                draw(ui);
            }
        });
        if !skip {
            // The scope's rect excludes its parent's trailing item spacing.
            // add_space reproduces precisely that rect on a skipped pass.
            let measured = response.response.rect.height();
            self.heights.insert(
                entry_id,
                MeasuredRow {
                    revision,
                    height: measured,
                },
            );
            if height != Some(measured) {
                // ScrollArea updates its sticky offset after laying out the
                // contents; paint that new offset on the next frame.
                ui.ctx().request_repaint();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout(ctx: &egui::Context, width: f32, entries: std::ops::Range<usize>) -> (usize, f32) {
        let mut drawn = 0;
        let mut extent = 0.0;
        let mut output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(width, 480.0),
                )),
                ..Default::default()
            },
            |ui| {
                let id = ui.id().with("list");
                let mut list = VirtualList::load(ui, id, entries.clone());
                // All rows are offscreen. Only unmeasured ones may draw.
                let viewport = egui::Rect::from_min_max(
                    egui::pos2(0.0, -2_000.0),
                    egui::pos2(width, -1_000.0),
                );
                for entry in entries.clone() {
                    list.row(ui, viewport, entry, 0, |ui| {
                        drawn += 1;
                        ui.add_space(3.0);
                        ui.label("variable height\nsecond line");
                        if entry % 2 == 0 {
                            ui.label("third line");
                        }
                    });
                }
                extent = ui.cursor().top();
                list.store(ui, id);
            },
        );
        output.textures_delta.clear();
        (drawn, extent)
    }

    #[test]
    fn unknown_rows_are_measured_and_spacers_preserve_the_layout_extent() {
        let ctx = egui::Context::default();
        set_transcript_virtualization_for_tests(&ctx, true);
        let (cold, expected) = layout(&ctx, 640.0, 0..20);
        assert_eq!(cold, 20);
        let (warm, actual) = layout(&ctx, 640.0, 0..20);
        assert_eq!(warm, 0);
        assert_eq!(actual, expected);

        // Appended offscreen rows must still be measured, not estimated.
        assert_eq!(layout(&ctx, 640.0, 0..21).0, 1);
        assert_eq!(layout(&ctx, 640.0, 0..21).0, 0);
        assert_eq!(layout(&ctx, 360.0, 0..21).0, 21);
        assert_eq!(layout(&ctx, 360.0, 0..21).0, 0);

        // Prune an evicted row; revisiting a view window measures it again.
        assert_eq!(layout(&ctx, 360.0, 1..21).0, 0);
        assert_eq!(layout(&ctx, 360.0, 0..21).0, 1);
    }
}
