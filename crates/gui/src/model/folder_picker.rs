use std::path::PathBuf;
use std::sync::{
    Arc,
    mpsc::{self, Receiver, TryRecvError},
};

pub trait FolderPicker {
    fn pick(&self) -> Receiver<Result<Option<PathBuf>, String>>;
}

pub struct PortalFolderPicker;

#[derive(Debug, thiserror::Error)]
enum PickerError {
    #[error("Folder dialog runtime: {0}")]
    Runtime(#[from] std::io::Error),
    #[error("Folder dialog: {0}")]
    Portal(#[from] ashpd::Error),
    #[error("Selected folder is not a local filesystem path")]
    NonLocal,
}

impl FolderPicker for PortalFolderPicker {
    fn pick(&self) -> Receiver<Result<Option<PathBuf>, String>> {
        let (tx, rx) = mpsc::channel();
        let failure_tx = tx.clone();
        let spawned = std::thread::Builder::new()
            .name("project-folder-picker".into())
            .spawn(move || {
                let result = pick_folder().map_err(|error| error.to_string());
                let _ = tx.send(result);
            });
        if let Err(error) = spawned {
            let _ = failure_tx.send(Err(error.to_string()));
        }
        rx
    }
}

fn pick_folder() -> Result<Option<PathBuf>, PickerError> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(async {
            let request = ashpd::desktop::file_chooser::OpenFileRequest::default()
                .title("Select project folder")
                .directory(true)
                .multiple(false)
                .send()
                .await?;
            let response = match request.response() {
                Ok(response) => response,
                Err(ashpd::Error::Response(ashpd::desktop::ResponseError::Cancelled)) => {
                    return Ok(None);
                }
                Err(error) => return Err(error.into()),
            };
            response
                .uris()
                .first()
                .map(|uri| {
                    url::Url::parse(uri.as_str())
                        .map_err(|_| PickerError::NonLocal)?
                        .to_file_path()
                        .map_err(|()| PickerError::NonLocal)
                })
                .transpose()
        })
}

#[derive(Default)]
pub struct FolderPickerModel {
    picker: Option<Arc<dyn FolderPicker>>,
    rx: Option<Receiver<Result<Option<PathBuf>, String>>>,
}

impl FolderPickerModel {
    pub fn new(picker: Arc<dyn FolderPicker>) -> Self {
        Self {
            picker: Some(picker),
            rx: None,
        }
    }

    pub fn start(&mut self) -> Result<(), String> {
        if self.is_busy() {
            return Ok(());
        }
        let picker = self
            .picker
            .as_ref()
            .ok_or("Folder dialog unavailable; type a path")?;
        self.rx = Some(picker.pick());
        Ok(())
    }

    pub fn poll(&mut self) -> Option<Result<Option<PathBuf>, String>> {
        let result = match self.rx.as_ref()?.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Empty) => return None,
            Err(TryRecvError::Disconnected) => Err("Folder dialog disconnected".into()),
        };
        self.rx = None;
        Some(result)
    }

    pub const fn is_busy(&self) -> bool {
        self.rx.is_some()
    }
}
