//! Opens presented artifacts with the desktop's default application.
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};

pub trait ArtifactOpener: Send + Sync {
    fn open(&self, path: &Path);
}

/// XDG Desktop Portal `OpenFile`: a file descriptor, never a `file://` URI the
/// portal would refuse, and never the in-app viewer that shows HTML source.
pub struct PortalArtifactOpener;

impl ArtifactOpener for PortalArtifactOpener {
    fn open(&self, path: &Path) {
        let path = path.to_path_buf();
        let spawned = std::thread::Builder::new()
            .name("artifact-opener".into())
            .spawn(move || {
                if let Err(error) = open_file(&path) {
                    tracing::warn!(%error, path = %path.display(), "failed to open artifact");
                }
            });
        if let Err(error) = spawned {
            tracing::warn!(%error, "failed to start artifact opener");
        }
    }
}

fn open_file(path: &Path) -> Result<(), String> {
    let file = std::fs::File::open(path).map_err(|error| error.to_string())?;
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?
        .block_on(async {
            ashpd::desktop::open_uri::OpenFileRequest::default()
                .ask(false)
                .send_file(&file)
                .await
                .map(drop)
                .map_err(|error| error.to_string())
        })
}

/// Queues HTML artifacts for the in-app preview and opens everything else, or
/// everything once the preview drops its receiver, with `fallback`.
pub struct HtmlPreviewOpener {
    previews: Sender<PathBuf>,
    fallback: Arc<dyn ArtifactOpener>,
}

impl HtmlPreviewOpener {
    pub fn new(fallback: Arc<dyn ArtifactOpener>) -> (Self, Receiver<PathBuf>) {
        let (previews, requests) = channel();
        (Self { previews, fallback }, requests)
    }
}

impl ArtifactOpener for HtmlPreviewOpener {
    fn open(&self, path: &Path) {
        if !is_html(path) || self.previews.send(path.to_path_buf()).is_err() {
            self.fallback.open(path);
        }
    }
}

pub fn is_html(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            extension.eq_ignore_ascii_case("html") || extension.eq_ignore_ascii_case("htm")
        })
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    #[derive(Default)]
    struct Recorder(Mutex<Vec<PathBuf>>);

    impl ArtifactOpener for Recorder {
        fn open(&self, path: &Path) {
            self.0.lock().unwrap().push(path.to_path_buf());
        }
    }

    #[test]
    fn html_goes_to_the_preview_and_other_files_to_the_fallback() {
        let fallback = Arc::new(Recorder::default());
        let (opener, requests) = HtmlPreviewOpener::new(fallback.clone());

        opener.open(Path::new("/a/report.html"));
        opener.open(Path::new("/a/PAGE.HTM"));
        opener.open(Path::new("/a/chart.png"));

        let queued: Vec<_> = requests.try_iter().collect();
        assert_eq!(
            queued,
            [
                PathBuf::from("/a/report.html"),
                PathBuf::from("/a/PAGE.HTM")
            ]
        );
        assert_eq!(*fallback.0.lock().unwrap(), [PathBuf::from("/a/chart.png")]);
    }

    #[test]
    fn html_falls_back_once_the_preview_is_gone() {
        let fallback = Arc::new(Recorder::default());
        let (opener, requests) = HtmlPreviewOpener::new(fallback.clone());
        drop(requests);

        opener.open(Path::new("/a/report.html"));

        assert_eq!(
            *fallback.0.lock().unwrap(),
            [PathBuf::from("/a/report.html")]
        );
    }
}
