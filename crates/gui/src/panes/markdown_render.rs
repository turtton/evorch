//! Markdown presentation for transcript text.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use egui::Ui;
use egui_commonmark::{CommonMarkCache, CommonMarkViewer};
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};

use crate::theme::tokens::palette;

mod cache;
mod table;

use cache::PreparedCache;

enum Block {
    Code(String),
    Table(table::Table),
}

/// Source-derived layout input for the viewer.
///
/// Preparing it parses the whole message, so it is cached across frames. The
/// only filesystem-dependent part is image URI rewriting, so `images` records
/// what it resolved and [`Prepared::is_current`] checks it before reuse.
struct Prepared {
    markdown: String,
    blocks: Vec<(String, Block)>,
    images: Vec<ImageLink>,
    image_uris: Vec<String>,
}

/// An image destination from the source and the path it resolved to.
struct ImageLink {
    destination: String,
    path: Option<PathBuf>,
}

impl ImageLink {
    fn resolve(destination: &str, base: Option<&Path>) -> Self {
        Self {
            destination: destination.to_owned(),
            path: super::file_viewer::resolve_file_link(destination, base).map(|link| link.path),
        }
    }
}

impl Prepared {
    /// Whether every image still resolves as it did when this was prepared
    /// (a symlink may have been retargeted, or a file created).
    fn is_current(&self, base: Option<&Path>) -> bool {
        self.images.iter().all(|image| {
            super::file_viewer::resolve_file_link(&image.destination, base).map(|link| link.path)
                == image.path
        })
    }

    /// Conservative heap bytes held, including container headers and spare
    /// capacity, so empty cells and many small blocks still count.
    fn bytes(&self) -> usize {
        use std::mem::size_of;
        let string = |value: &String| size_of::<String>() + value.capacity();
        let row = |row: &Vec<String>| {
            size_of::<Vec<String>>()
                + (row.capacity() - row.len()) * size_of::<String>()
                + row.iter().map(string).sum::<usize>()
        };
        let block = |(id, block): &(String, Block)| {
            string(id)
                + size_of::<Block>()
                + match block {
                    Block::Code(code) => code.capacity(),
                    Block::Table(table) => {
                        row(&table.header)
                            + (table.rows.capacity() - table.rows.len()) * size_of::<Vec<String>>()
                            + table.rows.iter().map(row).sum::<usize>()
                    }
                }
        };
        let image = |image: &ImageLink| {
            string(&image.destination)
                + size_of::<ImageLink>()
                + image.path.as_ref().map_or(0, |path| path.as_os_str().len())
        };
        size_of::<Self>()
            + self.markdown.capacity()
            + (self.blocks.capacity() - self.blocks.len()) * size_of::<(String, Block)>()
            + self.blocks.iter().map(block).sum::<usize>()
            + self.images.capacity() * size_of::<ImageLink>()
            + self.images.iter().map(image).sum::<usize>()
            + self.image_uris.capacity() * size_of::<String>()
            + self.image_uris.iter().map(String::capacity).sum::<usize>()
    }
}

thread_local! {
    static PREPARED: RefCell<PreparedCache> = RefCell::new(PreparedCache::default());
}

#[cfg(test)]
thread_local! {
    /// Number of full preprocessing passes run on this thread.
    static PREPARE_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

pub fn render_markdown(ui: &mut Ui, source: &str, id_salt: &str) {
    render_markdown_with_base(ui, source, id_salt, None, None);
}

/// `base` resolves relative links; `root` anchors the paths their menus copy.
pub fn render_markdown_with_base(
    ui: &mut Ui,
    source: &str,
    id_salt: &str,
    base: Option<&Path>,
    root: Option<&Path>,
) {
    super::file_viewer::install_image_loader(ui.ctx());
    let (link_base, link_root) = (base.map(Path::to_path_buf), root.map(Path::to_path_buf));
    let link_menu = move |ui: &mut Ui, destination: &str| {
        show_link_menu(ui, destination, link_base.as_deref(), link_root.as_deref());
    };
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
        let prepared = prepared_markdown(source, base);
        let table_link_menu = link_menu.clone();
        let markdown = prepared.markdown.as_str();
        let html_blocks = Arc::clone(&prepared);
        CommonMarkViewer::new()
            .link_menu_fn(Some(&link_menu))
            .render_html_fn(Some(&move |ui, html| {
                if let Some((id, Block::Table(table))) =
                    html_blocks.blocks.iter().find(|(id, _)| html.trim() == id)
                {
                    table::show(
                        ui,
                        id,
                        table,
                        ui.available_width().min(width),
                        &table_link_menu,
                    );
                    ui.end_row();
                } else if let Some((id, Block::Code(block))) =
                    html_blocks.blocks.iter().find(|(id, _)| html.trim() == id)
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
            .show(ui, &mut cache, markdown);
        for uri in &prepared.image_uris {
            super::agent::async_layout::observe_image(ui.ctx(), uri);
        }
    });
}

/// Cached [`prepare`] result for `source`, which must be rendered every frame.
fn prepared_markdown(source: &str, base: Option<&Path>) -> Arc<Prepared> {
    PREPARED.with(|cache| {
        cache
            .borrow_mut()
            .get_or_prepare(source, base, || prepare(source, base))
    })
}

/// Rewrites local images and splits code blocks and tables out of `source`.
fn prepare(source: &str, base: Option<&Path>) -> Prepared {
    #[cfg(test)]
    PREPARE_COUNT.with(|count| count.set(count.get() + 1));
    let (source, images) = prepare_local_images(source, base);
    let source = source.as_str();
    let mut start = 0;
    let mut block_start = None;
    let mut blocks = Vec::new();
    let mut image_uris = Vec::new();
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
            Event::Start(Tag::Image { dest_url, .. }) => {
                let uri = dest_url.into_string();
                // Match the viewer's implicit file scheme: observing a bare
                // path would miss the loader actually used by markdown.
                image_uris.push(if uri.contains("://") || uri.starts_with("data:") {
                    uri
                } else {
                    format!("file://{uri}")
                });
            }
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
    Prepared {
        markdown,
        blocks,
        images,
        image_uris,
    }
}

fn show_link_menu(ui: &mut Ui, destination: &str, base: Option<&Path>, root: Option<&Path>) {
    if let Some(link) = super::file_viewer::resolve_file_link(destination, base) {
        if ui.button("Open").clicked() {
            ui.ctx().open_url(egui::OpenUrl::same_tab(destination));
            ui.close();
        }
        super::file_viewer::copy_path_items(ui, &link.path, root);
        return;
    }
    if ui.button("Open in browser").clicked() {
        ui.ctx().open_url(egui::OpenUrl::same_tab(destination));
        ui.close();
    }
    if ui.button("Copy URL").clicked() {
        ui.ctx().copy_text(destination.to_owned());
        ui.close();
    }
}

fn prepare_local_images(source: &str, base: Option<&Path>) -> (String, Vec<ImageLink>) {
    if base.is_none() {
        return (source.to_owned(), Vec::new());
    }
    let mut images = Vec::new();
    let mut result = String::new();
    let mut image = None;
    let mut start = 0;
    for (event, range) in Parser::new_ext(source, Options::ENABLE_TABLES).into_offset_iter() {
        match event {
            Event::Start(Tag::Image { dest_url, .. }) => {
                let link = ImageLink::resolve(&dest_url, base);
                image = link
                    .path
                    .as_deref()
                    .and_then(super::file_viewer::image_uri)
                    .map(|uri| (range.start, uri, String::new()));
                images.push(link);
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
    (result, images)
}

#[cfg(test)]
mod tests;
