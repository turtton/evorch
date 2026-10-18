//! In-app HTML artifact previews on CEF off-screen rendering (ADR 0034 B).
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::Receiver;
use std::time::Duration;

use browser_cef::offline::ArtifactBundle;
use browser_cef::{
    ArtifactContext, CefRuntime, CefView, NetLog, OsrBrowser, RuntimeConfig, render_snapshot,
};

use crate::model::artifact_opener::ArtifactOpener;
use crate::model::html_snapshots::{HtmlSnapshots, SnapshotWorker, read_html};

const DEFAULT_SIZE: [f32; 2] = [960.0, 640.0];
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(15);

/// Preview windows for the paths [`crate::model::artifact_opener::HtmlPreviewOpener`]
/// queues, and the renderer for card snapshots. CEF starts on the first
/// request; if it cannot, the receiver is dropped, every artifact goes to
/// `fallback` and snapshots fail.
pub struct HtmlPreviewWindows {
    previews: Vec<Preview>,
    requests: Option<Receiver<PathBuf>>,
    fallback: Arc<dyn ArtifactOpener>,
    snapshots: Arc<HtmlSnapshots>,
    next_id: u64,
    init_error: Option<String>,
    snapshot_timeout: Duration,
    snapshot_worker: Option<SnapshotWorker>,
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
    pub fn new(
        requests: Receiver<PathBuf>,
        fallback: Arc<dyn ArtifactOpener>,
        snapshots: Arc<HtmlSnapshots>,
    ) -> Self {
        Self {
            previews: Vec::new(),
            requests: Some(requests),
            fallback,
            snapshots,
            next_id: 0,
            init_error: None,
            snapshot_timeout: SNAPSHOT_TIMEOUT,
            snapshot_worker: None,
            runtime: None,
        }
    }

    /// Lets conversation cards request snapshots from this renderer.
    pub fn install_snapshots(&self, ctx: &egui::Context) {
        Arc::clone(&self.snapshots).install(ctx);
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
        self.render_snapshots(ctx);
        for preview in &mut self.previews {
            preview.show(ctx);
        }
        self.previews.retain(|preview| preview.open);
    }

    fn render_snapshots(&mut self, ctx: &egui::Context) {
        let requests = self.snapshots.take_requests();
        if requests.is_empty() {
            return;
        }
        match self.snapshot_worker(ctx) {
            Ok(worker) => requests
                .into_iter()
                .for_each(|request| worker.submit(request)),
            Err(error) => {
                for request in requests {
                    self.snapshots.finish(&request.sha256, Err(error.clone()));
                }
            }
        }
    }

    fn snapshot_worker(&mut self, ctx: &egui::Context) -> Result<&SnapshotWorker, String> {
        self.ensure_runtime()?;
        if self.snapshot_worker.is_none() {
            let timeout = self.snapshot_timeout;
            let worker =
                SnapshotWorker::spawn(Arc::clone(&self.snapshots), ctx.clone(), move |html| {
                    render_snapshot(html, timeout)
                })?;
            self.snapshot_worker = Some(worker);
        }
        self.snapshot_worker
            .as_ref()
            .ok_or_else(|| "the snapshot worker did not start".into())
    }

    fn ensure_runtime(&mut self) -> Result<(), String> {
        if self.runtime.is_some() {
            return Ok(());
        }
        if let Some(error) = &self.init_error {
            return Err(error.clone());
        }
        match CefRuntime::initialize(RuntimeConfig::offline()) {
            Ok(runtime) => {
                self.runtime = Some(runtime);
                Ok(())
            }
            Err(error) => {
                self.requests = None;
                self.init_error = Some(error.clone());
                Err(error)
            }
        }
    }

    fn open(&mut self, path: &Path, pixels_per_point: f32) -> Result<(), String> {
        let html = read_html(path)?;
        self.ensure_runtime()?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::artifact_opener::{HtmlPreviewOpener, PortalArtifactOpener};
    use crate::model::html_snapshots::SnapshotState;

    #[test]
    #[ignore = "needs the CEF runtime and EVORCH_CEF_HELPER; see docs/browser.md"]
    fn card_snapshot_requests_render_through_cef() {
        let dir = tempfile::tempdir().unwrap();
        let html = dir.path().join("mock.html");
        std::fs::write(&html, "<!doctype html><h1>mock</h1>").unwrap();
        let sha256 = "cd".repeat(32);
        let snapshots = Arc::new(HtmlSnapshots::new(dir.path().join("snapshots")));
        let (_opener, requests) = HtmlPreviewOpener::new(Arc::new(PortalArtifactOpener));
        let mut windows = HtmlPreviewWindows::new(
            requests,
            Arc::new(PortalArtifactOpener),
            Arc::clone(&snapshots),
        );
        // nextest's slow-timeout bounds the render; it has no other deadline.
        windows.snapshot_timeout = Duration::MAX;
        assert_eq!(snapshots.state(&sha256, &html), SnapshotState::Pending);

        windows.render(&egui::Context::default());
        // Joins the snapshot worker before CEF shuts down.
        drop(windows);

        let SnapshotState::Ready(png) = snapshots.state(&sha256, &html) else {
            panic!("{:?}", snapshots.state(&sha256, &html));
        };
        let image = image::open(png).unwrap();
        assert_eq!(
            (image.width(), image.height()),
            (
                browser_cef::SNAPSHOT_SIZE[0] as u32,
                browser_cef::SNAPSHOT_SIZE[1] as u32
            )
        );
    }
}
