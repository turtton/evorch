//! Opens presented artifacts with the desktop's default application.
use std::path::Path;

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
