use crate::theme::tokens::palette;

pub(super) fn show(ui: &mut egui::Ui, id: egui::Id, content: (&str, bool)) {
    let (text, streaming) = content;
    let previous = ui.data_mut(|data| {
        let previous = data.get_temp::<bool>(id);
        data.insert_temp(id, streaming);
        previous
    });
    let transition = previous
        .filter(|previous| *previous != streaming)
        .map(|_| streaming);
    egui::CollapsingHeader::new(egui::RichText::new("thinking").color(palette().TEXT_MUTED))
        .id_salt(id)
        .default_open(streaming)
        .open(transition)
        .show(ui, |ui| {
            ui.visuals_mut().override_text_color = Some(palette().TEXT_MUTED);
            let layer = ui.layer_id();
            let start = ui
                .ctx()
                .graphics_mut(|graphics| graphics.entry(layer).next_idx());
            crate::panes::markdown_render::render_markdown(ui, text, "thinking-body");
            // CommonMark has no base italic style; relayout only this body's text.
            let shapes = ui.ctx().graphics_mut(|graphics| {
                graphics
                    .entry(layer)
                    .all_entries()
                    .enumerate()
                    .skip(start.0)
                    .filter_map(|(index, clipped)| match &clipped.shape {
                        egui::epaint::Shape::Text(shape) => Some((
                            egui::layers::ShapeIdx(index),
                            clipped.clip_rect,
                            shape.clone(),
                        )),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
            });
            for (index, clip, mut shape) in shapes {
                let mut job = shape.galley.job.as_ref().clone();
                for section in &mut job.sections {
                    section.format.italics = true;
                }
                shape.galley = ui.fonts_mut(|fonts| fonts.layout_job(job));
                ui.ctx().graphics_mut(|graphics| {
                    graphics
                        .entry(layer)
                        .set(index, clip, egui::epaint::Shape::Text(shape));
                });
            }
        });
}
