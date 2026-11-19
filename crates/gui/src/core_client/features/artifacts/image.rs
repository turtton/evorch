//! Decoding an image artifact under limits of its own.
//!
//! The compressed bytes are already bounded by the data plane
//! (`MAX_IMAGE_BLOB_BYTES`), but a few kilobytes of PNG can declare an image of
//! gigabytes, or carry a compressed ICC profile or text chunk that expands to
//! hundreds of MiB while the decoder is *created*. The limits are therefore
//! handed to the reader before the decoder exists (the PNG decoder takes its
//! memory budget at creation and a later `set_limits` does not change it), the
//! size is checked from the header before any pixel is decoded, and the decoder
//! keeps an allocation limit while it decodes. Decoding runs on a thread of its
//! own (at most one at a time, see the feature); only the texture upload is the
//! GUI thread's.
use std::{
    io::Cursor,
    sync::mpsc::{Receiver, TryRecvError, channel},
    thread::JoinHandle,
};
use workbench_protocol::namespaces::browser::data::MediaType;

/// The pixels of a 4K frame: what the data plane already treats as the largest picture.
pub(super) const MAX_PIXELS: u64 = 3840 * 2160;
/// A texture side of every GPU the desktop runs on.
pub(super) const MAX_SIDE: u32 = 8192;
/// The decoded size of the image itself, 16-bit samples included.
pub(super) const MAX_DECODED_BYTES: u64 = 64 * 1024 * 1024;
/// Everything the decoder may allocate: the pixels and its row buffers.
pub(super) const MAX_ALLOC: u64 = 2 * MAX_DECODED_BYTES;

#[derive(Debug, PartialEq, Eq)]
pub(super) enum ImageProblem {
    /// The header or the decoder's budget says more than the limits allow.
    TooLarge(String),
    Decode(String),
}

fn format(media: MediaType) -> image::ImageFormat {
    match media {
        MediaType::Png => image::ImageFormat::Png,
        MediaType::Jpeg => image::ImageFormat::Jpeg,
        MediaType::Gif => image::ImageFormat::Gif,
        MediaType::Webp => image::ImageFormat::WebP,
    }
}

fn problem(error: image::ImageError) -> ImageProblem {
    match error {
        image::ImageError::Limits(limit) => ImageProblem::TooLarge(limit.to_string()),
        other => ImageProblem::Decode(other.to_string()),
    }
}

/// The chunks a PNG needs for its pixels. Compressed ancillary chunks (iCCP, zTXt,
/// iTXt) and the animation chunks are dropped before a decoder is created, so
/// nothing that expands is ever handed to it; the kept chunks are copied
/// unchanged (their CRCs stay valid) and the decoder still checks them.
pub(super) fn strip_png(bytes: &[u8]) -> Result<Vec<u8>, ImageProblem> {
    const SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    const KEEP: [&[u8; 4]; 5] = [b"IHDR", b"PLTE", b"tRNS", b"IDAT", b"IEND"];
    let malformed = |why: &str| ImageProblem::Decode(format!("PNG: {why}"));
    if !bytes.starts_with(&SIGNATURE) {
        return Err(malformed("bad signature"));
    }
    let mut out = Vec::with_capacity(bytes.len());
    out.extend_from_slice(&SIGNATURE);
    let mut at = SIGNATURE.len();
    while at + 12 <= bytes.len() {
        let length = u32::from_be_bytes(bytes[at..at + 4].try_into().expect("four bytes")) as usize;
        let end = at
            .checked_add(12)
            .and_then(|head| head.checked_add(length))
            .filter(|end| *end <= bytes.len())
            .ok_or_else(|| malformed("truncated chunk"))?;
        let kind: &[u8] = &bytes[at + 4..at + 8];
        if KEEP.iter().any(|keep| keep.as_slice() == kind) {
            out.extend_from_slice(&bytes[at..end]);
        }
        at = end;
        if kind == b"IEND" {
            return Ok(out);
        }
    }
    Err(malformed("no end chunk"))
}

/// Decode `bytes` as `media`: the first frame of an animation, as RGBA.
pub(super) fn decode(bytes: &[u8], media: MediaType) -> Result<egui::ColorImage, ImageProblem> {
    decode_within(bytes, media, MAX_ALLOC)
}

pub(super) fn decode_within(
    bytes: &[u8],
    media: MediaType,
    max_alloc: u64,
) -> Result<egui::ColorImage, ImageProblem> {
    use image::ImageDecoder;
    let mut limits = image::Limits::no_limits();
    limits.max_image_width = Some(MAX_SIDE);
    limits.max_image_height = Some(MAX_SIDE);
    limits.max_alloc = Some(max_alloc);
    let stripped;
    let bytes = if media == MediaType::Png {
        stripped = strip_png(bytes)?;
        stripped.as_slice()
    } else {
        bytes
    };
    let mut reader = image::ImageReader::with_format(Cursor::new(bytes), format(media));
    reader.limits(limits);
    let decoder = reader.into_decoder().map_err(problem)?;
    let (width, height) = decoder.dimensions();
    if u64::from(width) * u64::from(height) > MAX_PIXELS
        || decoder.total_bytes() > MAX_DECODED_BYTES
    {
        return Err(ImageProblem::TooLarge(format!(
            "{width}×{height} ピクセル（上限 {MAX_SIDE} 辺・{MAX_PIXELS} ピクセル）"
        )));
    }
    let rgba = image::DynamicImage::from_decoder(decoder)
        .map_err(problem)?
        .into_rgba8();
    Ok(egui::ColorImage::from_rgba_unmultiplied(
        [width as usize, height as usize],
        rgba.as_raw(),
    ))
}

/// A decode running on its own thread.
pub(super) struct Job {
    /// The artifact view the image was asked for; a result for another is dropped.
    pub(super) serial: u64,
    /// Gone once the job is abandoned: the thread then drops its own result.
    receiver: Option<Receiver<Result<egui::ColorImage, ImageProblem>>>,
    handle: Option<JoinHandle<()>>,
}

impl Job {
    /// Start decoding `bytes`; the thread asks `ctx` for a repaint when it is done.
    pub(super) fn spawn(serial: u64, bytes: Vec<u8>, media: MediaType, ctx: egui::Context) -> Self {
        let (sender, receiver) = channel();
        let spawned = std::thread::Builder::new()
            .name("artifact-image-decode".to_owned())
            .spawn({
                let sender = sender.clone();
                move || {
                    // An abandoned job has no receiver: the image is dropped here.
                    let _ = sender.send(decode(&bytes, media));
                    ctx.request_repaint();
                }
            });
        let handle = match spawned {
            Ok(handle) => Some(handle),
            Err(error) => {
                let _ = sender.send(Err(ImageProblem::Decode(error.to_string())));
                None
            }
        };
        Self {
            serial,
            receiver: Some(receiver),
            handle,
        }
    }

    /// Nobody waits for the result any more. The thread is not interrupted (a
    /// decode cannot be), but it frees its result itself instead of leaving it
    /// in the channel, and the job stays until the thread is done so that a new
    /// decode never runs beside it.
    pub(super) fn abandon(&mut self) {
        self.receiver = None;
    }

    /// The result if the thread is done and someone waits for it. A thread that
    /// died without one is a failure.
    pub(super) fn poll(&self) -> Option<Result<egui::ColorImage, ImageProblem>> {
        match self.receiver.as_ref()?.try_recv() {
            Ok(result) => Some(result),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                Some(Err(ImageProblem::Decode("decoder stopped".to_owned())))
            }
        }
    }

    pub(super) fn abandoned(&self) -> bool {
        self.receiver.is_none()
    }

    /// The thread has ended (or never started).
    pub(super) fn finished(&self) -> bool {
        self.handle.as_ref().is_none_or(JoinHandle::is_finished)
    }

    /// Wait for the result (tests wait on the thread itself, never on a clock).
    #[cfg(test)]
    pub(super) fn wait(self) -> Result<egui::ColorImage, ImageProblem> {
        self.receiver
            .expect("not abandoned")
            .recv()
            .unwrap_or_else(|_| Err(ImageProblem::Decode("decoder stopped".to_owned())))
    }

    /// Wait for the thread to end (tests).
    #[cfg(test)]
    pub(super) fn join(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.join().expect("the decoder thread does not panic");
        }
    }
}
