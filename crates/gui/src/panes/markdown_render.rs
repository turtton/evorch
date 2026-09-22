//! Markdown presentation for transcript text.

use egui::Ui;
use egui_commonmark::{CommonMarkCache, CommonMarkViewer};
use pulldown_cmark::{Event, Options, Parser, Tag};

use crate::theme::tokens::palette;

pub fn render_markdown(ui: &mut Ui, source: &str, id_salt: &str) {
    ui.push_id(id_salt, |ui| {
        let color = ui.visuals().override_text_color.unwrap_or(palette().TEXT);
        ui.visuals_mut().widgets.noninteractive.fg_stroke.color = color;
        ui.visuals_mut().widgets.active.fg_stroke.color = color;
        ui.visuals_mut().extreme_bg_color = palette().SURFACE_RAISED;
        let width = ui.available_width();
        ui.set_max_width(width);
        ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Wrap);
        let mut cache = CommonMarkCache::default();
        let mut start = 0;
        let mut depth = 0;
        let mut block_start = 0;
        let mut scroll_block = false;
        for (event, range) in Parser::new_ext(source, Options::ENABLE_TABLES).into_offset_iter() {
            match event {
                Event::Start(tag) => {
                    if depth == 0 {
                        block_start = range.start;
                    }
                    depth += 1;
                    scroll_block |= matches!(tag, Tag::CodeBlock(_) | Tag::Table(_));
                }
                Event::End(_) => {
                    depth -= 1;
                    if depth == 0 && scroll_block {
                        CommonMarkViewer::new().show(ui, &mut cache, &source[start..block_start]);
                        egui::ScrollArea::horizontal()
                            .id_salt(block_start)
                            .max_width(width)
                            .show(ui, |ui| {
                                let block = &source[block_start..range.end];
                                let natural_width = ui.fonts_mut(|fonts| {
                                    fonts
                                        .layout_no_wrap(
                                            block.to_owned(),
                                            egui::TextStyle::Monospace.resolve(ui.style()),
                                            color,
                                        )
                                        .size()
                                        .x
                                });
                                ui.set_width(
                                    (natural_width + crate::theme::tokens::SP_4).max(width),
                                );
                                CommonMarkViewer::new().show(ui, &mut cache, block);
                            });
                        start = range.end;
                        scroll_block = false;
                    }
                }
                _ => {}
            }
        }
        CommonMarkViewer::new().show(ui, &mut cache, &source[start..]);
    });
}

#[cfg(test)]
mod tests;
