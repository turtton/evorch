//! Bounded, background-only file/clipboard intake, independent of chat models.
use std::{
    io::{Cursor, Read},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
    thread::JoinHandle,
};

use image::{ImageBuffer, ImageFormat, Rgba};
use workbench_protocol::conversation::ATTACHMENT_FILE_BYTES as FILE_BYTES;

#[derive(Clone, PartialEq, Eq)]
pub struct AttachmentFile {
    pub draft_id: String,
    pub filename: String,
    pub media_type: String,
    pub bytes: Vec<u8>,
}

impl std::fmt::Debug for AttachmentFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AttachmentFile")
            .field("draft_id", &self.draft_id)
            .field("byte_length", &self.bytes.len())
            .finish_non_exhaustive()
    }
}

#[cfg(target_os = "linux")]
mod wayland;

const READ_IMAGE_BYTES: usize = 3 * 1024 * 1024 + 768 * 1024;
const READ_IMAGE_EDGE: u32 = 8000;
const MAX_RGBA_BYTES: usize = 256 * 1024 * 1024;

/// This boundary is replaceable in headless tests. It never runs on the UI thread.
pub trait ClipboardBackend: Send {
    fn image(&mut self) -> Result<ClipboardRgba, String>;
}

pub struct ClipboardRgba {
    pub width: usize,
    pub height: usize,
    pub bytes: Vec<u8>,
}

struct NativeClipboard {
    #[cfg(target_os = "linux")]
    wayland_display: Option<usize>,
    #[cfg(target_os = "linux")]
    core: Option<wayland::CoreClipboard>,
}

impl NativeClipboard {
    fn new(display: Option<usize>) -> Self {
        #[cfg(not(target_os = "linux"))]
        let _ = display;
        Self {
            #[cfg(target_os = "linux")]
            wayland_display: display,
            #[cfg(target_os = "linux")]
            core: None,
        }
    }
}

impl ClipboardBackend for NativeClipboard {
    fn image(&mut self) -> Result<ClipboardRgba, String> {
        #[cfg(target_os = "linux")]
        if let Some(display) = self.wayland_display {
            // SAFETY: configure_window captures this live eframe display. The
            // clipboard worker is joined before it can outlive the window.
            if self.core.is_none() {
                self.core = Some(unsafe { wayland::CoreClipboard::new(display) }?);
            }
            return self
                .core
                .as_mut()
                .expect("Wayland clipboard initialized")
                .image();
        }
        let image = arboard::Clipboard::new()
            .and_then(|mut clipboard| clipboard.get_image())
            .map_err(|error| format!("Clipboard image: {error}"))?;
        Ok(ClipboardRgba {
            width: image.width,
            height: image.height,
            bytes: image.bytes.into_owned(),
        })
    }
}

/// PNG bytes are bounded by the read tool's actual image contract, not by the
/// larger generic-file limit. Even noisy, high-resolution RGBA is reduced.
pub fn clipboard_png(source: ClipboardRgba) -> Result<AttachmentFile, String> {
    let length = source
        .width
        .checked_mul(source.height)
        .and_then(|n| n.checked_mul(4));
    if length != Some(source.bytes.len())
        || source.bytes.len() > MAX_RGBA_BYTES
        || source.width == 0
        || source.height == 0
    {
        return Err("Invalid or excessively large clipboard RGBA image".into());
    }
    let width = u32::try_from(source.width).map_err(|_| "Clipboard width overflow")?;
    let height = u32::try_from(source.height).map_err(|_| "Clipboard height overflow")?;
    let image = ImageBuffer::<Rgba<u8>, _>::from_raw(width, height, source.bytes)
        .ok_or("Invalid clipboard RGBA length")?;
    let mut image = image::DynamicImage::ImageRgba8(image);
    if width.max(height) > READ_IMAGE_EDGE {
        image = image.resize(
            READ_IMAGE_EDGE,
            READ_IMAGE_EDGE,
            image::imageops::FilterType::Triangle,
        );
    }
    loop {
        let mut bytes = Cursor::new(Vec::new());
        image
            .write_to(&mut bytes, ImageFormat::Png)
            .map_err(|e| format!("Clipboard PNG: {e}"))?;
        if bytes.get_ref().len() <= READ_IMAGE_BYTES {
            return Ok(AttachmentFile {
                draft_id: uuid::Uuid::new_v4().to_string(),
                filename: "clipboard.png".into(),
                media_type: "image/png".into(),
                bytes: bytes.into_inner(),
            });
        }
        let (width, height) = (image.width(), image.height());
        if width == 1 && height == 1 {
            return Err("Clipboard image cannot fit the read image limit".into());
        }
        image = image.resize_exact(
            (width * 3 / 4).max(1),
            (height * 3 / 4).max(1),
            image::imageops::FilterType::Triangle,
        );
    }
}

fn file(path: &Path) -> Result<AttachmentFile, String> {
    let mut file = std::fs::File::open(path).map_err(|e| format!("Attachment open: {e}"))?;
    let metadata = file
        .metadata()
        .map_err(|e| format!("Attachment metadata: {e}"))?;
    if !metadata.is_file() {
        return Err("Attachments must be ordinary files; directories are not imported".into());
    }
    if metadata.len() > FILE_BYTES {
        return Err("Attachment exceeds 8 MiB".into());
    }
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or("Attachment filename is not UTF-8")?
        .to_owned();
    if filename.len() > 255 || filename.chars().any(char::is_control) {
        return Err("Attachment filename is too long or contains control characters".into());
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    (&mut file)
        .take(FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("Attachment read: {e}"))?;
    if bytes.len() as u64 > FILE_BYTES {
        return Err("Attachment grew beyond 8 MiB while reading".into());
    }
    let media_type = match path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("pdf") => "application/pdf",
        Some("txt" | "md" | "rs" | "json" | "csv") => "text/plain",
        _ => "application/octet-stream",
    };
    Ok(AttachmentFile {
        draft_id: uuid::Uuid::new_v4().to_string(),
        filename,
        media_type: media_type.into(),
        bytes,
    })
}

pub enum Intake {
    Files(Vec<PathBuf>),
    ChooseFiles,
    Clipboard,
}

pub struct IntakeResult {
    pub thread: String,
    pub request_id: String,
    pub files: Result<Vec<AttachmentFile>, String>,
}

struct Request {
    thread: String,
    request_id: String,
    intake: Intake,
    repaint: egui::Context,
}

struct ClipboardWorker {
    sender: Option<SyncSender<Request>>,
    handle: Option<JoinHandle<()>>,
    stop: Arc<AtomicBool>,
}

impl ClipboardWorker {
    fn new(mut backend: Box<dyn ClipboardBackend>, result: mpsc::Sender<IntakeResult>) -> Self {
        let (sender, receiver) = mpsc::sync_channel::<Request>(32);
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let handle = std::thread::spawn(move || {
            while let Ok(request) = receiver.recv() {
                if worker_stop.load(Ordering::Acquire) {
                    break;
                }
                let files = backend
                    .image()
                    .and_then(clipboard_png)
                    .map(|file| vec![file]);
                complete(request, files, |value| {
                    let _ = result.send(value);
                });
            }
        });
        Self {
            sender: Some(sender),
            handle: Some(handle),
            stop,
        }
    }
}

impl Drop for ClipboardWorker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.sender.take();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn complete(
    request: Request,
    files: Result<Vec<AttachmentFile>, String>,
    result: impl FnOnce(IntakeResult),
) {
    result(IntakeResult {
        thread: request.thread,
        request_id: request.request_id,
        files,
    });
    request.repaint.request_repaint();
}

/// File selection can wait for the user without retaining a borrowed Wayland
/// display. Clipboard work has its own joined worker for display lifetime safety.
pub struct AttachmentIo {
    files: Option<SyncSender<Request>>,
    clipboard: Option<ClipboardWorker>,
    result: mpsc::Sender<IntakeResult>,
    receiver: Receiver<IntakeResult>,
    file_result: SyncSender<IntakeResult>,
    file_receiver: Receiver<IntakeResult>,
    stop: Arc<AtomicBool>,
    display_key: Option<Option<usize>>,
}

impl Default for AttachmentIo {
    fn default() -> Self {
        let (result, receiver) = mpsc::channel();
        let (file_result, file_receiver) = mpsc::sync_channel(1);
        Self {
            files: None,
            clipboard: None,
            result,
            receiver,
            file_result,
            file_receiver,
            stop: Arc::new(AtomicBool::new(false)),
            display_key: None,
        }
    }
}

impl AttachmentIo {
    pub fn configure_window(&mut self, frame: &eframe::Frame) {
        use raw_window_handle::{HasDisplayHandle, RawDisplayHandle};
        let Ok(display) = frame.display_handle() else {
            return;
        };
        let display = match display.as_raw() {
            RawDisplayHandle::Wayland(handle) => Some(handle.display.as_ptr() as usize),
            _ => None,
        };
        if self.display_key != Some(display) {
            self.clipboard.take();
            self.display_key = Some(display);
            self.clipboard = Some(ClipboardWorker::new(
                Box::new(NativeClipboard::new(display)),
                self.result.clone(),
            ));
        }
    }

    pub fn set_clipboard_backend(&mut self, backend: Box<dyn ClipboardBackend>) {
        self.clipboard.take();
        self.clipboard = Some(ClipboardWorker::new(backend, self.result.clone()));
    }

    pub fn submit(
        &mut self,
        thread: &str,
        intake: Intake,
        repaint: &egui::Context,
    ) -> Result<String, String> {
        let request_id = uuid::Uuid::new_v4().to_string();
        let request = Request {
            thread: thread.into(),
            request_id: request_id.clone(),
            intake,
            repaint: repaint.clone(),
        };
        let sender = if matches!(request.intake, Intake::Clipboard) {
            self.clipboard
                .as_ref()
                .and_then(|worker| worker.sender.as_ref())
                .ok_or("Clipboard is unavailable before the native window is ready")?
        } else {
            if self.files.is_none() {
                let (sender, receiver) = mpsc::sync_channel::<Request>(32);
                let stop = self.stop.clone();
                let result = self.file_result.clone();
                std::thread::Builder::new()
                    .name("attachment-import".into())
                    .spawn(move || {
                        while let Ok(request) = receiver.recv() {
                            if stop.load(Ordering::Acquire) {
                                break;
                            }
                            let paths = match &request.intake {
                                Intake::Files(paths) => Ok(paths.clone()),
                                Intake::ChooseFiles => choose_files(),
                                Intake::Clipboard => unreachable!(),
                            };
                            let files = paths.and_then(|paths| {
                                if paths.len() > 32 {
                                    return Err("Select at most 32 attachments".into());
                                }
                                let mut total = 0_u64;
                                paths
                                    .into_iter()
                                    .map(|path| {
                                        let file = file(&path)?;
                                        total += file.bytes.len() as u64;
                                        if total > 64 * 1024 * 1024 {
                                            return Err("Selected attachments exceed 64 MiB".into());
                                        }
                                        Ok(file)
                                    })
                                    .collect()
                            });
                            complete(request, files, |value| {
                                let _ = result.send(value);
                            });
                        }
                    })
                    .map_err(|e| e.to_string())?;
                self.files = Some(sender);
            }
            self.files.as_ref().expect("file worker started")
        };
        sender
            .try_send(request)
            .map_err(|_| "Attachment preparation queue is full".to_owned())?;
        Ok(request_id)
    }

    pub fn poll(&self) -> impl Iterator<Item = IntakeResult> + '_ {
        self.receiver
            .try_iter()
            .chain(self.file_receiver.try_iter())
    }

    pub fn shutdown_clipboard(&mut self) {
        self.clipboard.take();
    }
}

impl Drop for AttachmentIo {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.files.take();
        self.clipboard.take();
    }
}

fn choose_files() -> Result<Vec<PathBuf>, String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?
        .block_on(async {
            let request = ashpd::desktop::file_chooser::OpenFileRequest::default()
                .title("Attach files")
                .directory(false)
                .multiple(true)
                .send()
                .await
                .map_err(|e| format!("Attachment file picker: {e}"))?;
            let response = match request.response() {
                Ok(response) => response,
                Err(ashpd::Error::Response(ashpd::desktop::ResponseError::Cancelled)) => {
                    return Ok(Vec::new());
                }
                Err(error) => return Err(format!("Attachment file picker: {error}")),
            };
            response
                .uris()
                .iter()
                .map(|uri| {
                    url::Url::parse(uri.as_str())
                        .map_err(|e| e.to_string())?
                        .to_file_path()
                        .map_err(|()| "Attachment must be a local file".into())
                })
                .collect()
        })
}
