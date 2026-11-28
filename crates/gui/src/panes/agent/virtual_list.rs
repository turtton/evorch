//! Measured, variable-height transcript rows. Unknown rows are never estimated:
//! cold/width-change passes measure every row until its height settles.

fn measurement_id(ctx: &egui::Context) -> egui::Id {
    egui::Id::new("transcript-layout-async").with(ctx.viewport_id())
}

/// Record external layout dependencies through nested child renderers.
pub(crate) fn mark_async_layout(ctx: &egui::Context) -> bool {
    let id = measurement_id(ctx);
    ctx.data_mut(|data| {
        if data.get_temp::<bool>(id).is_some() {
            data.insert_temp(id, true);
            true
        } else {
            false
        }
    })
}
