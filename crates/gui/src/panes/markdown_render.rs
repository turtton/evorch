//! Markdown presentation for transcript text.

use std::path::Path;

use egui::Ui;
use egui_commonmark::{CommonMarkCache, CommonMarkViewer};
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};

use crate::theme::tokens::palette;

mod table;

enum Block {
    Code(String),
    Table(table::Table),
}

pub fn render_markdown(ui: &mut Ui, source: &str, id_salt: &str) {
    render_markdown_with_base(ui, source, id_salt, None);
}

pub fn render_markdown_with_base(ui: &mut Ui, source: &str, id_salt: &str, base: Option<&Path>) {
    super::file_viewer::install_image_loader(ui.ctx());
    let source = prepare_local_images(source, base);
    let source = source.as_str();
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
        let mut table = table::Table::default();
        let mut in_head = false;
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
                Event::Start(Tag::Table(_)) => {
                    block_start = Some((range.start, TagEnd::Table));
                }
                Event::Start(Tag::TableHead) => in_head = true,
                Event::End(TagEnd::TableHead) => in_head = false,
                Event::Start(Tag::TableRow) => table.rows.push(Vec::new()),
                Event::Start(Tag::TableCell) => {
                    let cell = table::cell_source(&source[range]);
                    match table.rows.last_mut() {
                        Some(row) if !in_head => row.push(cell),
                        _ => table.header.push(cell),
                    }
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
                        let content = if end == TagEnd::Table {
                            Block::Table(std::mem::take(&mut table))
                        } else {
                            Block::Code(std::mem::take(&mut block))
                        };
                        blocks.push((placeholder, content));
                        start = range.end;
                    }
                }
                _ => {}
            }
        }
        markdown.push_str(&source[start..]);
        CommonMarkViewer::new()
            .render_html_fn(Some(&move |ui, html| {
                if let Some((id, Block::Table(table))) =
                    blocks.iter().find(|(id, _)| html.trim() == id)
                {
                    table::show(ui, id, table, ui.available_width().min(width));
                    ui.end_row();
                } else if let Some((id, Block::Code(block))) =
                    blocks.iter().find(|(id, _)| html.trim() == id)
                {
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

fn prepare_local_images(source: &str, base: Option<&Path>) -> String {
    if base.is_none() {
        return source.to_owned();
    }
    let mut result = String::new();
    let mut image = None;
    let mut start = 0;
    for (event, range) in Parser::new_ext(source, Options::ENABLE_TABLES).into_offset_iter() {
        match event {
            Event::Start(Tag::Image { dest_url, .. }) => {
                image = super::file_viewer::resolve_file_link(&dest_url, base)
                    .and_then(|link| super::file_viewer::image_uri(&link.path))
                    .map(|uri| (range.start, uri, String::new()));
            }
            Event::Text(text) if image.is_some() => {
                if let Some((_, _, alt)) = &mut image {
                    alt.push_str(&text);
                }
            }
            Event::End(TagEnd::Image) => {
                if let Some((open, uri, alt)) = image.take() {
                    result.push_str(&source[start..open]);
                    let alt = alt
                        .replace('\\', "\\\\")
                        .replace('[', "\\[")
                        .replace(']', "\\]");
                    result.push_str(&format!("![{alt}]({uri})"));
                    start = range.end;
                }
            }
            _ => {}
        }
    }
    result.push_str(&source[start..]);
    result
}

#[cfg(test)]
mod tests;
