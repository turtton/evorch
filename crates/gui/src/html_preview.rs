//! In-app HTML artifact previews on CEF off-screen rendering (ADR 0034 B).
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::Receiver;

use browser_cef::offline::ArtifactBundle;
use browser_cef::{ArtifactContext, CefRuntime, CefView, NetLog, OsrBrowser, RuntimeConfig};

use crate::model::artifact_opener::ArtifactOpener;

const DEFAULT_SIZE: [f32; 2] = [960.0, 640.0];

/// Preview windows for the paths [`crate::model::artifact_opener::HtmlPreviewOpener`]
/// queues. CEF starts on the first request; if it cannot, the receiver is
/// dropped and every artifact goes to `fallback`.
pub struct HtmlPreviewWindows {
    previews: Vec<Preview>,
    requests: Option<Receiver<PathBuf>>,
    fallback: Arc<dyn ArtifactOpener>,
    next_id: u64,
    // Declared last so it drops after every browser has closed.
    runtime: Option<CefRuntime>,
}

struct Preview {
    id: u64,
    name: String,
    view: CefView,
    log: Arc<NetLog>,
    open: bool,
}

impl HtmlPreviewWindows {
    pub fn new(requests: Receiver<PathBuf>, fallback: Arc<dyn ArtifactOpener>) -> Self {
        Self {
            previews: Vec::new(),
            requests: Some(requests),
            fallback,
            next_id: 0,
            runtime: None,
        }
    }

    pub fn render(&mut self, ctx: &egui::Context) {
        let requested: Vec<PathBuf> = self
            .requests
            .as_ref()
            .map(|requests| requests.try_iter().collect())
            .unwrap_or_default();
        for path in requested {
            if let Err(error) = self.open(&path, ctx.pixels_per_point()) {
                tracing::warn!(%error, path = %path.display(), "HTML preview unavailable");
                self.fallback.open(&path);
            }
        }
        for preview in &mut self.previews {
            preview.show(ctx);
        }
        self.previews.retain(|preview| preview.open);
    }

    fn open(&mut self, path: &Path, pixels_per_point: f32) -> Result<(), String> {
        let html = read_html(path)?;
        if self.runtime.is_none() {
            let runtime = CefRuntime::initialize(RuntimeConfig::offline()).inspect_err(|_| {
                self.requests = None;
            })?;
            self.runtime = Some(runtime);
        }
        let context = ArtifactContext::create(ArtifactBundle::html(html))?;
        let log = Arc::clone(&context.log);
        let size = DEFAULT_SIZE.map(|value| value as i32);
        let browser = OsrBrowser::create(
            context,
            &ArtifactContext::url("index.html"),
            size,
            pixels_per_point,
        )?;
        self.next_id += 1;
        self.previews.push(Preview {
            id: self.next_id,
            name: path.file_name().map_or_else(
                || path.display().to_string(),
                |name| name.to_string_lossy().into_owned(),
            ),
            view: CefView::new(browser),
            log,
            open: true,
        });
        Ok(())
    }
}

fn read_html(path: &Path) -> Result<Vec<u8>, String> {
    let length = std::fs::metadata(path)
        .map_err(|error| error.to_string())?
        .len();
    if length > runtime::artifacts::MAX_HTML_BYTES {
        return Err(format!(
            "{length} bytes exceeds the {} byte HTML artifact limit",
            runtime::artifacts::MAX_HTML_BYTES
        ));
    }
    std::fs::read(path).map_err(|error| error.to_string())
}

impl Preview {
    fn show(&mut self, ctx: &egui::Context) {
        let (title, crash, blocked) = {
            let status = self.view.browser().status();
            let title = if status.title.is_empty() {
                self.name.clone()
            } else {
                format!("{} — {}", status.title, self.name)
            };
            let blocked = status.blocked_navigations.len()
                + status.blocked_popups.len()
                + self
                    .log
                    .blocked
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .len();
            (title, status.last_crash.clone(), blocked)
        };
        let view = &mut self.view;
        egui::Window::new(title)
            .id(egui::Id::new(("html-preview", self.id)))
            .open(&mut self.open)
            .default_size(DEFAULT_SIZE)
            .resizable(true)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    if ui.button("Reload").clicked() {
                        view.browser().reload();
                    }
                    ui.label("Offline preview");
                    if blocked > 0 {
                        ui.weak(format!("{blocked} network requests blocked"));
                    }
                    if let Some(crash) = crash {
                        ui.colored_label(
                            ui.visuals().warn_fg_color,
                            format!("Renderer crashed ({crash})"),
                        );
                    }
                });
                view.ui(ui);
            });
    }
}
