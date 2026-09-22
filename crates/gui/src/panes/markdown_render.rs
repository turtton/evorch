//! Markdown presentation for transcript text.

use egui::Ui;
use egui_commonmark::{CommonMarkCache, CommonMarkViewer};
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};

use crate::theme::tokens::palette;

pub fn render_markdown(ui: &mut Ui, source: &str, id_salt: &str) {
    ui.push_id(id_salt, |ui| {
        let color = ui.visuals().override_text_color.unwrap_or(palette().TEXT);
        ui.visuals_mut().widgets.noninteractive.fg_stroke.color = color;
        ui.visuals_mut().widgets.active.fg_stroke.color = color;
        ui.visuals_mut().extreme_bg_color = palette().SURFACE_RAISED;
        // Leave room for glyph ink extending beyond the logical wrapping width.
        let width = (ui.available_width() - crate::theme::tokens::SP_1).max(0.0);
        ui.set_width(width);
        ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Wrap);
        let mut cache = CommonMarkCache::default();
        let mut start = 0;
        let mut block_start = None;
        let mut blocks = Vec::new();
        let mut markdown = String::new();
        let mut block = String::new();
        let mut table_separator = String::new();
        let mut fence = String::from("~~~");
        while source.contains(&fence) {
            fence.push('~');
        }
        let mut marker = String::from("evorch-scroll");
        while source.contains(&marker) {
            marker.push('-');
        }
        for (event, range) in Parser::new_ext(source, Options::ENABLE_TABLES).into_offset_iter() {
            match event {
                Event::Start(Tag::CodeBlock(kind)) => {
                    block_start = Some((range.start, TagEnd::CodeBlock));
                    block.push_str(&fence);
                    if let pulldown_cmark::CodeBlockKind::Fenced(language) = kind {
                        block.push_str(&language);
                    }
                    block.push('\n');
                }
                Event::Start(Tag::Table(alignments)) => {
                    block_start = Some((range.start, TagEnd::Table));
                    table_separator = alignments
                        .iter()
                        .map(|alignment| match alignment {
                            pulldown_cmark::Alignment::None => "| --- ",
                            pulldown_cmark::Alignment::Left => "| :--- ",
                            pulldown_cmark::Alignment::Center => "| :---: ",
                            pulldown_cmark::Alignment::Right => "| ---: ",
                        })
                        .collect::<String>();
                    table_separator.push_str("|\n");
                }
                Event::Start(Tag::TableHead | Tag::TableRow) => {
                    block.push_str(source[range].trim_end());
                    block.push('\n');
                }
                Event::End(TagEnd::TableHead) => {
                    block.push_str(&table_separator);
                }
                Event::Text(text) if matches!(block_start, Some((_, TagEnd::CodeBlock))) => {
                    block.push_str(&text);
                }
                Event::End(end @ (TagEnd::CodeBlock | TagEnd::Table)) => {
                    if let Some((open, _)) = block_start.take() {
                        if end == TagEnd::CodeBlock {
                            if !block.ends_with('\n') {
                                block.push('\n');
                            }
                            block.push_str(&fence);
                        }
                        // Replace only this element; retain the enclosing list/quote syntax
                        // so the viewer lays out all surrounding prose in one normal pass.
                        markdown.push_str(&source[start..open]);
                        let placeholder = format!("<!--{marker}-{}-->", blocks.len());
                        markdown.push_str(&placeholder);
                        markdown.push('\n');
                        blocks.push((placeholder, std::mem::take(&mut block)));
                        start = range.end;
                    }
                }
                _ => {}
            }
        }
        markdown.push_str(&source[start..]);
        CommonMarkViewer::new()
            .render_html_fn(Some(&move |ui, html| {
                if let Some((id, block)) = blocks.iter().find(|(id, _)| html.trim() == id) {
                    let width = ui.available_width().min(width);
                    ui.allocate_ui_with_layout(
                        egui::vec2(width, 0.0),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| {
                            egui::ScrollArea::horizontal()
                                .id_salt(id)
                                .max_width(width)
                                .show(ui, |ui| {
                                    ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
                                    let natural_width = ui.fonts_mut(|fonts| {
                                        fonts
                                            .layout_no_wrap(
                                                block.clone(),
                                                egui::TextStyle::Monospace.resolve(ui.style()),
                                                color,
                                            )
                                            .size()
                                            .x
                                    });
                                    ui.with_layout(
                                        egui::Layout::top_down(egui::Align::Min),
                                        |ui| {
                                            ui.set_width(
                                                (natural_width + crate::theme::tokens::SP_4)
                                                    .max(width),
                                            );
                                            CommonMarkViewer::new().show(
                                                ui,
                                                &mut CommonMarkCache::default(),
                                                block,
                                            );
                                        },
                                    );
                                });
                        },
                    );
                    ui.end_row();
                } else {
                    CommonMarkViewer::new().show(ui, &mut CommonMarkCache::default(), html);
                }
            }))
            .show(ui, &mut cache, &markdown);
    });
}

#[cfg(test)]
mod tests;
