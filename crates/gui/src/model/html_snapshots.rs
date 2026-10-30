//! PNG snapshots of HTML artifacts for conversation cards (ADR 0034 B).
//!
//! Snapshots are cached by the artifact's content hash, so a cached file never
//! goes stale. Cards ask for a state; the host drains the requests into a
//! [`SnapshotWorker`] with a renderer (CEF in `html_preview`). Without an
//! installed [`HtmlSnapshots`], cards keep their text-only HTML entry.
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use egui::ColorImage;

#[derive(Clone, Debug, PartialEq)]
pub enum SnapshotState {
    Pending,
    Ready(PathBuf),
    Failed(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct SnapshotRequest {
    pub sha256: String,
    pub path: PathBuf,
}

pub struct HtmlSnapshots {
    dir: PathBuf,
    states: Mutex<HashMap<String, SnapshotState>>,
    requests: Mutex<Vec<SnapshotRequest>>,
    generation: AtomicU64,
}

impl HtmlSnapshots {
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            states: Mutex::default(),
            requests: Mutex::default(),
            generation: AtomicU64::new(0),
        }
    }

    pub fn install(self: Arc<Self>, ctx: &egui::Context) {
        ctx.data_mut(|data| data.insert_temp(Self::id(), self));
    }

    pub fn installed(ctx: &egui::Context) -> Option<Arc<Self>> {
        ctx.data(|data| data.get_temp::<Arc<Self>>(Self::id()))
    }

    fn id() -> egui::Id {
        egui::Id::new("html-snapshots")
    }

    /// The snapshot for an artifact; the first miss queues a render request.
    pub fn state(&self, sha256: &str, path: &Path) -> SnapshotState {
        if !is_sha256(sha256) {
            return SnapshotState::Failed("invalid content hash".into());
        }
        let mut states = lock(&self.states);
        if let Some(state) = states.get(sha256) {
            return state.clone();
        }
        let cached = self.cache_path(sha256);
        let state = if cached.is_file() {
            SnapshotState::Ready(cached)
        } else {
            lock(&self.requests).push(SnapshotRequest {
                sha256: sha256.to_owned(),
                path: path.to_path_buf(),
            });
            SnapshotState::Pending
        };
        states.insert(sha256.to_owned(), state.clone());
        state
    }

    pub fn take_requests(&self) -> Vec<SnapshotRequest> {
        std::mem::take(&mut *lock(&self.requests))
    }

    pub fn finish(&self, sha256: &str, rendered: Result<ColorImage, String>) {
        let state = match rendered.and_then(|image| self.store(sha256, &image)) {
            Ok(path) => SnapshotState::Ready(path),
            Err(error) => {
                tracing::warn!(%error, sha256, "HTML snapshot failed");
                SnapshotState::Failed(error)
            }
        };
        lock(&self.states).insert(sha256.to_owned(), state);
        self.generation.fetch_add(1, Ordering::Release);
    }

    pub(crate) fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    fn cache_path(&self, sha256: &str) -> PathBuf {
        self.dir.join(format!("{sha256}-v1.png"))
    }

    fn store(&self, sha256: &str, image: &ColorImage) -> Result<PathBuf, String> {
        let [width, height] = image.size;
        let pixels = image
            .pixels
            .iter()
            .flat_map(|pixel| pixel.to_srgba_unmultiplied())
            .collect();
        let png = image::RgbaImage::from_raw(width as u32, height as u32, pixels)
            .ok_or("snapshot size does not match its pixels")?;
        std::fs::create_dir_all(&self.dir).map_err(|error| error.to_string())?;
        let file = tempfile::Builder::new()
            .suffix(".png")
            .tempfile_in(&self.dir)
            .map_err(|error| error.to_string())?;
        png.save_with_format(file.path(), image::ImageFormat::Png)
            .map_err(|error| error.to_string())?;
        let path = self.cache_path(sha256);
        file.persist(&path).map_err(|error| error.to_string())?;
        Ok(path)
    }
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

pub fn read_html(path: &Path) -> Result<Vec<u8>, String> {
    // Stored HTML may exceed the source limit once its images are inlined (ADR 0038).
    let limit = runtime::artifacts::MAX_BUNDLED_HTML_BYTES;
    let length = std::fs::metadata(path)
        .map_err(|error| error.to_string())?
        .len();
    if length > limit {
        return Err(format!(
            "{length} bytes exceeds the {limit} byte HTML artifact limit"
        ));
    }
    std::fs::read(path).map_err(|error| error.to_string())
}

/// Renders requests one at a time on its own thread. Dropping it finishes the
/// request in progress and joins the thread, so a renderer that needs a
/// runtime can be dropped before that runtime.
pub struct SnapshotWorker {
    requests: Option<Sender<SnapshotRequest>>,
    thread: Option<JoinHandle<()>>,
}

impl SnapshotWorker {
    pub fn spawn(
        snapshots: Arc<HtmlSnapshots>,
        repaint: egui::Context,
        mut render: impl FnMut(Vec<u8>) -> Result<ColorImage, String> + Send + 'static,
    ) -> Result<Self, String> {
        let (requests, received) = channel::<SnapshotRequest>();
        let thread = std::thread::Builder::new()
            .name("html-snapshots".into())
            .spawn(move || {
                for request in received {
                    let rendered = read_html(&request.path).and_then(&mut render);
                    snapshots.finish(&request.sha256, rendered);
                    crate::repaint::coalesced(&repaint);
                }
            })
            .map_err(|error| error.to_string())?;
        Ok(Self {
            requests: Some(requests),
            thread: Some(thread),
        })
    }

    pub fn submit(&self, request: SnapshotRequest) {
        if let Some(requests) = &self.requests {
            let _ = requests.send(request);
        }
    }
}

impl Drop for SnapshotWorker {
    fn drop(&mut self) {
        self.requests.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use egui::Color32;

    use super::*;

    const SHA: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn a_miss_queues_one_request_and_a_render_is_cached_by_content_hash() {
        let dir = tempfile::tempdir().unwrap();
        let snapshots = HtmlSnapshots::new(dir.path().join("cache"));
        let html = Path::new("/artifacts/mock.html");

        assert_eq!(snapshots.state(SHA, html), SnapshotState::Pending);
        assert_eq!(snapshots.state(SHA, html), SnapshotState::Pending);
        assert_eq!(
            snapshots.take_requests(),
            [SnapshotRequest {
                sha256: SHA.into(),
                path: html.into()
            }]
        );

        snapshots.finish(SHA, Ok(ColorImage::filled([3, 2], Color32::BLUE)));

        let SnapshotState::Ready(png) = snapshots.state(SHA, html) else {
            panic!("snapshot not stored");
        };
        let stored = image::open(&png).unwrap().to_rgba8();
        assert_eq!(stored.dimensions(), (3, 2));
        assert_eq!(stored.get_pixel(2, 1).0, [0, 0, 255, 255]);
        let restarted = HtmlSnapshots::new(dir.path().join("cache"));
        assert_eq!(restarted.state(SHA, html), SnapshotState::Ready(png));
        assert!(restarted.take_requests().is_empty());
    }

    #[test]
    fn failures_and_invalid_hashes_are_reported_without_retrying() {
        let dir = tempfile::tempdir().unwrap();
        let snapshots = HtmlSnapshots::new(dir.path().to_path_buf());
        let html = Path::new("/artifacts/mock.html");

        assert!(matches!(
            snapshots.state("../escape", html),
            SnapshotState::Failed(_)
        ));
        snapshots.state(SHA, html);
        snapshots.finish(SHA, Err("CEF unavailable".into()));

        assert_eq!(
            snapshots.state(SHA, html),
            SnapshotState::Failed("CEF unavailable".into())
        );
        assert_eq!(snapshots.take_requests().len(), 1);
    }

    #[test]
    fn stored_html_with_inlined_images_is_read_up_to_the_bundled_limit() {
        let dir = tempfile::tempdir().unwrap();
        let bundled = dir.path().join("bundled.html");
        let file = std::fs::File::create(&bundled).unwrap();
        file.set_len(runtime::artifacts::MAX_HTML_BYTES + 1)
            .unwrap();
        assert!(read_html(&bundled).is_ok());
        file.set_len(runtime::artifacts::MAX_BUNDLED_HTML_BYTES + 1)
            .unwrap();
        assert!(read_html(&bundled).unwrap_err().contains("exceeds"));
    }

    #[test]
    fn the_worker_renders_the_artifact_file_and_finishes_before_it_drops() {
        let dir = tempfile::tempdir().unwrap();
        let html = dir.path().join("mock.html");
        std::fs::write(&html, "<p>mock</p>").unwrap();
        let snapshots = Arc::new(HtmlSnapshots::new(dir.path().join("cache")));
        snapshots.state(SHA, &html);
        let rendered = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&rendered);
        let worker = SnapshotWorker::spawn(
            Arc::clone(&snapshots),
            egui::Context::default(),
            move |body| {
                seen.lock().unwrap().push(body);
                Ok(ColorImage::filled([1, 1], Color32::WHITE))
            },
        )
        .unwrap();

        for request in snapshots.take_requests() {
            worker.submit(request);
        }
        drop(worker);

        assert_eq!(*rendered.lock().unwrap(), [b"<p>mock</p>".to_vec()]);
        assert!(matches!(
            snapshots.state(SHA, &html),
            SnapshotState::Ready(_)
        ));
    }
}
