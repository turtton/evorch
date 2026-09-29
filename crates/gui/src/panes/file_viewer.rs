//! Bounded, read-only previews for local files linked from conversations.

use std::collections::BTreeMap;
use std::io::{Cursor, Read};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use egui::{ColorImage, TextureHandle};
use egui_extras::syntax_highlighting::{CodeTheme, highlight};

const MAX_FILE_BYTES: u64 = 32 * 1024 * 1024;
const MAX_TEXT_BYTES: usize = 2 * 1024 * 1024;
const MAX_IMAGE_SIDE: u32 = 8192;
const MAX_IMAGE_PIXELS: u64 = 16 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileLink {
    pub path: PathBuf,
    pub line: Option<usize>,
}

/// External URLs and document-only anchors stay with the normal link handler.
/// Explicit absolute paths may point outside the project, as in editor links.
pub fn resolve_file_link(link: &str, base: Option<&Path>) -> Option<FileLink> {
    let link = link.trim();
    if link.is_empty() || link.starts_with('#') || link.starts_with("//") {
        return None;
    }
    let (raw, anchor) = link.split_once('#').unwrap_or((link, ""));
    let mut line = anchor
        .strip_prefix('L')
        .or_else(|| anchor.strip_prefix("line="))
        .and_then(|value| value.split(['-', ':']).next()?.parse().ok());
    let raw = if raw.starts_with("file://") {
        let url = url::Url::parse(raw).ok()?;
        url.to_file_path().ok()?
    } else {
        // A final :line or :line:column is an editor location, not a URI scheme.
        let (path, location) = split_location(raw);
        line = line.or(location);
        if path.contains(':') && url::Url::parse(path).is_ok() {
            return None;
        }
        PathBuf::from(percent_decode(path)?)
    };
    let (path, location) = split_location(raw.to_str()?);
    line = line.or(location).filter(|line| *line > 0);
    let path = PathBuf::from(path);
    let path = if path.is_absolute() {
        path
    } else {
        base?.join(path)
    };
    let path = path
        .canonicalize()
        .unwrap_or_else(|_| normalize_path(&path));
    Some(FileLink { path, line })
}

fn split_location(path: &str) -> (&str, Option<usize>) {
    let Some((prefix, suffix)) = path.rsplit_once(':') else {
        return (path, None);
    };
    let Ok(last) = suffix.parse::<usize>() else {
        return (path, None);
    };
    if let Some((path, line)) = prefix.rsplit_once(':')
        && let Ok(line) = line.parse()
    {
        return (path, Some(line));
    }
    (prefix, Some(last))
}

fn percent_decode(value: &str) -> Option<String> {
    let mut bytes = Vec::with_capacity(value.len());
    let mut input = value.bytes();
    while let Some(byte) = input.next() {
        if byte == b'%' {
            let high = (input.next()? as char).to_digit(16)?;
            let low = (input.next()? as char).to_digit(16)?;
            bytes.push((high * 16 + low) as u8);
        } else {
            bytes.push(byte);
        }
    }
    String::from_utf8(bytes).ok()
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                result.pop();
            }
            component => result.push(component.as_os_str()),
        }
    }
    result
}

/// Consume only local links emitted by this pane, preserving external commands.
pub(crate) fn take_file_links(
    ctx: &egui::Context,
    from: usize,
    base: Option<&Path>,
) -> Vec<FileLink> {
    ctx.output_mut(|output| {
        let mut links = Vec::new();
        let mut index = 0;
        output.commands.retain(|command| {
            let eligible = index >= from;
            index += 1;
            if eligible
                && let egui::OutputCommand::OpenUrl(url) = command
                && let Some(link) = resolve_file_link(&url.url, base)
            {
                links.push(link);
                return false;
            }
            true
        });
        links
    })
}

#[derive(Clone)]
enum LoadedFile {
    Text(Arc<str>),
    Image(Arc<ColorImage>),
}

type LoadResult = Result<LoadedFile, String>;

#[derive(Clone)]
struct FileCache {
    result: Arc<Mutex<Option<LoadResult>>>,
    texture: Option<TextureHandle>,
}

fn cache_id(path: &Path) -> egui::Id {
    egui::Id::new(("file-preview-cache", path))
}
fn line_id(path: &Path) -> egui::Id {
    egui::Id::new(("file-preview-line", path))
}

pub(crate) fn request_line(ctx: &egui::Context, link: &FileLink) {
    if let Some(line) = link.line {
        ctx.data_mut(|data| data.insert_temp(line_id(&link.path), line));
    }
}

pub(crate) fn forget_file(ctx: &egui::Context, path: &Path) {
    let cache = ctx.data_mut(|data| {
        let cache = data.get_temp::<FileCache>(cache_id(path));
        data.remove::<FileCache>(cache_id(path));
        cache
    });
    if let Some(cache) = cache {
        let loaded = cache
            .result
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        if let Some(Ok(LoadedFile::Text(source))) = loaded {
            for event in pulldown_cmark::Parser::new(&source) {
                if let pulldown_cmark::Event::Start(pulldown_cmark::Tag::Image { dest_url, .. }) =
                    event
                    && let Some(link) = resolve_file_link(&dest_url, path.parent())
                    && let Some(uri) = image_uri(&link.path)
                {
                    ctx.forget_image(&uri);
                }
            }
        }
    }
    if let Some(uri) = image_uri(path) {
        ctx.forget_image(&uri);
    }
}

fn cached_file(ctx: &egui::Context, path: &Path) -> FileCache {
    if let Some(cache) = ctx.data_mut(|data| data.get_temp::<FileCache>(cache_id(path))) {
        return cache;
    }
    let cache = start_file_load(ctx, path);
    ctx.data_mut(|data| data.insert_temp(cache_id(path), cache.clone()));
    cache
}

fn start_file_load(ctx: &egui::Context, path: &Path) -> FileCache {
    let cache = FileCache {
        result: Arc::new(Mutex::new(None)),
        texture: None,
    };
    let result = cache.result.clone();
    let path_owned = path.to_path_buf();
    let ctx_owned = ctx.clone();
    std::thread::spawn(move || {
        let loaded = load_file(&path_owned);
        *result.lock().unwrap_or_else(|error| error.into_inner()) = Some(loaded);
        ctx_owned.request_repaint();
    });
    cache
}

const IMAGE_SCHEME: &str = "evorch-preview://";

pub(crate) fn image_uri(path: &Path) -> Option<String> {
    url::Url::from_file_path(path)
        .ok()
        .map(|url| format!("{IMAGE_SCHEME}{url}"))
}

pub(crate) fn install_image_loader(ctx: &egui::Context) {
    let loader = LocalImageLoader::default();
    if !ctx.is_loader_installed(egui::load::ImageLoader::id(&loader)) {
        ctx.add_image_loader(Arc::new(loader));
    }
}

#[derive(Default)]
struct LocalImageLoader {
    cache: Mutex<BTreeMap<String, FileCache>>,
}

impl egui::load::ImageLoader for LocalImageLoader {
    fn id(&self) -> &str {
        concat!(module_path!(), "::LocalImageLoader")
    }

    fn load(
        &self,
        ctx: &egui::Context,
        uri: &str,
        _: egui::load::SizeHint,
    ) -> egui::load::ImageLoadResult {
        use egui::load::{ImagePoll, LoadError};
        let file_uri = uri
            .strip_prefix(IMAGE_SCHEME)
            .ok_or(LoadError::NotSupported)?;
        let path = url::Url::parse(file_uri)
            .ok()
            .and_then(|url| url.to_file_path().ok())
            .ok_or_else(|| LoadError::Loading("画像のパスが無効です。".into()))?;
        let mut cache = self.cache.lock().unwrap_or_else(|error| error.into_inner());
        let file = cache
            .entry(uri.to_owned())
            .or_insert_with(|| start_file_load(ctx, &path));
        match file
            .result
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
        {
            Some(Ok(LoadedFile::Image(image))) => Ok(ImagePoll::Ready { image }),
            Some(Ok(LoadedFile::Text(_))) => {
                Err(LoadError::Loading("画像形式ではありません。".into()))
            }
            Some(Err(error)) => Err(LoadError::Loading(error)),
            None => Ok(ImagePoll::Pending { size: None }),
        }
    }

    fn forget(&self, uri: &str) {
        self.cache
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(uri);
    }

    fn forget_all(&self) {
        self.cache
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clear();
    }

    fn byte_size(&self) -> usize {
        self.cache
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .values()
            .map(|file| {
                match file
                    .result
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .as_ref()
                {
                    Some(Ok(LoadedFile::Image(image))) => image.pixels.len() * 4,
                    Some(Ok(LoadedFile::Text(text))) => text.len(),
                    Some(Err(error)) => error.len(),
                    None => 0,
                }
            })
            .sum()
    }
}

fn load_file(path: &Path) -> LoadResult {
    let metadata = path
        .metadata()
        .map_err(|error| format!("ファイルを開けません: {error}"))?;
    if !metadata.is_file() {
        return Err("通常のファイルを選択してください。".into());
    }
    if metadata.len() > MAX_FILE_BYTES {
        return Err("32 MiB を超えるファイルは表示できません。".into());
    }
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .and_then(|file| file.take(MAX_FILE_BYTES + 1).read_to_end(&mut bytes))
        .map_err(|error| format!("ファイルを読み込めません: {error}"))?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err("32 MiB を超えるファイルは表示できません。".into());
    }
    if let Ok(format) = image::guess_format(&bytes) {
        let dimensions = image::ImageReader::with_format(Cursor::new(&bytes), format)
            .into_dimensions()
            .map_err(|error| format!("画像を読み込めません: {error}"))?;
        if dimensions.0 > MAX_IMAGE_SIDE
            || dimensions.1 > MAX_IMAGE_SIDE
            || u64::from(dimensions.0) * u64::from(dimensions.1) > MAX_IMAGE_PIXELS
        {
            return Err("画像は一辺 8192 px、合計 16 メガピクセルまで表示できます。".into());
        }
        let mut reader = image::ImageReader::with_format(Cursor::new(&bytes), format);
        let mut limits = image::Limits::default();
        limits.max_image_width = Some(MAX_IMAGE_SIDE);
        limits.max_image_height = Some(MAX_IMAGE_SIDE);
        limits.max_alloc = Some(128 * 1024 * 1024);
        reader.limits(limits);
        let image = reader
            .decode()
            .map_err(|error| format!("画像を読み込めません: {error}"))?
            .to_rgba8();
        return Ok(LoadedFile::Image(Arc::new(
            ColorImage::from_rgba_unmultiplied(
                [image.width() as usize, image.height() as usize],
                image.as_raw(),
            ),
        )));
    }
    if bytes.len() > MAX_TEXT_BYTES {
        return Err("テキストは 2 MiB まで表示できます。".into());
    }
    if bytes.contains(&0) {
        return Err("このバイナリ形式はプレビューできません。".into());
    }
    let text = String::from_utf8(bytes)
        .map_err(|_| "UTF-8 以外のテキスト、または未対応のバイナリ形式です。".to_owned())?;
    Ok(LoadedFile::Text(text.into()))
}

/// Local Markdown images use the same bounded loader as standalone previews.
pub(crate) fn local_image_ui(ui: &mut egui::Ui, path: &Path) {
    let mut cache = cached_file(ui.ctx(), path);
    let loaded = cache
        .result
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clone();
    match loaded {
        Some(Ok(LoadedFile::Image(image))) => {
            let texture = cache.texture.get_or_insert_with(|| {
                ui.ctx().load_texture(
                    path.display().to_string(),
                    image.as_ref().clone(),
                    egui::TextureOptions::LINEAR,
                )
            });
            ui.add(
                egui::Image::new(&*texture)
                    .max_width(ui.available_width())
                    .shrink_to_fit(),
            );
            ui.ctx()
                .data_mut(|data| data.insert_temp(cache_id(path), cache));
        }
        Some(Ok(LoadedFile::Text(_))) => {
            ui.label("画像形式ではありません。");
        }
        Some(Err(error)) => {
            ui.colored_label(ui.visuals().error_fg_color, error);
        }
        None => {
            ui.label("画像を読み込み中…");
            ui.ctx().request_repaint_after(Duration::from_millis(50));
        }
    }
}

pub fn file_viewer_pane(ui: &mut egui::Ui, path: &Path) {
    ui.push_id(path, |ui| {
        ui.horizontal(|ui| {
            if ui.small_button("再読み込み").clicked() {
                forget_file(ui.ctx(), path);
            }
            ui.add(egui::Label::new(path.display().to_string()).truncate())
                .on_hover_text(path.display().to_string());
        });
        ui.separator();
        let cache = cached_file(ui.ctx(), path);
        let loaded = cache
            .result
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        match loaded {
            None => {
                ui.label("ファイルを読み込み中…");
                ui.ctx().request_repaint_after(Duration::from_millis(50));
            }
            Some(Err(error)) => {
                ui.colored_label(ui.visuals().error_fg_color, error);
            }
            Some(Ok(LoadedFile::Image(_))) => {
                egui::ScrollArea::both()
                    .id_salt("image")
                    .show(ui, |ui| local_image_ui(ui, path));
            }
            Some(Ok(LoadedFile::Text(text))) => {
                let extension = path
                    .extension()
                    .and_then(|value| value.to_str())
                    .unwrap_or("")
                    .to_ascii_lowercase();
                let is_markdown = matches!(extension.as_str(), "md" | "markdown" | "mdown");
                let source_id = ui.id().with("show-source");
                let pending_line = ui
                    .ctx()
                    .data_mut(|data| data.get_temp::<usize>(line_id(path)));
                let mut source = ui
                    .ctx()
                    .data_mut(|data| data.get_temp::<bool>(source_id))
                    .unwrap_or(!is_markdown);
                if pending_line.is_some() {
                    source = true;
                }
                if is_markdown {
                    ui.horizontal(|ui| {
                        ui.selectable_value(&mut source, false, "プレビュー");
                        ui.selectable_value(&mut source, true, "ソース");
                    });
                }
                ui.ctx()
                    .data_mut(|data| data.insert_temp(source_id, source));
                if source {
                    let theme = CodeTheme::from_style(ui.style());
                    let row_height = ui.text_style_height(&egui::TextStyle::Monospace);
                    let lines: Vec<_> = text.lines().collect();
                    let mut scroll = egui::ScrollArea::both().id_salt("source");
                    if let Some(line) = pending_line {
                        let line = line.min(lines.len().max(1));
                        scroll = scroll
                            .vertical_scroll_offset((line.saturating_sub(1) as f32) * row_height);
                        ui.ctx()
                            .data_mut(|data| data.remove::<usize>(line_id(path)));
                    }
                    ui.spacing_mut().item_spacing.y = 0.0;
                    scroll.show_rows(ui, row_height, lines.len(), |ui, range| {
                        ui.spacing_mut().item_spacing.y = 0.0;
                        for index in range {
                            ui.horizontal(|ui| {
                                ui.monospace(format!("{:>5}", index + 1));
                                let job = highlight(
                                    ui.ctx(),
                                    ui.style(),
                                    &theme,
                                    lines[index],
                                    &extension,
                                );
                                ui.add(egui::Label::new(job).selectable(true).extend());
                            });
                        }
                    });
                } else {
                    egui::ScrollArea::vertical()
                        .id_salt("markdown")
                        .show(ui, |ui| {
                            super::markdown_render::render_markdown_with_base(
                                ui,
                                &text,
                                "file-markdown",
                                path.parent(),
                            );
                        });
                }
            }
        }
    });
}

#[cfg(test)]
mod tests;
