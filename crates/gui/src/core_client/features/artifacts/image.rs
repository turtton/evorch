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

/// A WebP checked before a decoder exists. The decoder sizes its buffers from the
/// frame's own header and compares it with the canvas only afterwards, so a 48 byte
/// file whose canvas says 1×1 and whose frame says 16383×16383 makes it allocate
/// 256 MiB. Every dimension a decoder will meet (canvas, animation frame, VP8 and
/// VP8L frame) is read here, must be within the limits and, for a still image,
/// must be the canvas. What cannot be read is refused.
pub(super) fn check_webp(bytes: &[u8]) -> Result<(), ImageProblem> {
    let malformed = |why: &str| ImageProblem::Decode(format!("WebP: {why}"));
    if bytes.len() < 12 || &bytes[..4] != b"RIFF" || &bytes[8..12] != b"WEBP" {
        return Err(malformed("not a RIFF WebP"));
    }
    let mut canvas = None;
    let mut at = 12;
    while at + 8 <= bytes.len() {
        let kind: [u8; 4] = bytes[at..at + 4].try_into().expect("four bytes");
        let length = u32::from_le_bytes(bytes[at + 4..at + 8].try_into().expect("four bytes"));
        let start = at + 8;
        let end = usize::try_from(length)
            .ok()
            .and_then(|length| start.checked_add(length))
            .filter(|end| *end <= bytes.len())
            .ok_or_else(|| malformed("truncated chunk"))?;
        let payload = &bytes[start..end];
        match &kind {
            b"VP8X" => {
                if payload.len() < 10 {
                    return Err(malformed("short VP8X"));
                }
                let size = (u24(&payload[4..7]) + 1, u24(&payload[7..10]) + 1);
                within_limits(size)?;
                canvas = Some(size);
            }
            b"VP8 " | b"VP8L" => frame_chunk(&kind, payload, canvas, None)?,
            b"ANMF" => {
                if payload.len() < 16 {
                    return Err(malformed("short ANMF"));
                }
                let size = (u24(&payload[6..9]) + 1, u24(&payload[9..12]) + 1);
                within_limits(size)?;
                if canvas.is_some_and(|canvas| size.0 > canvas.0 || size.1 > canvas.1) {
                    return Err(malformed("animation frame larger than the canvas"));
                }
                let mut inner = 16;
                while inner + 8 <= payload.len() {
                    let kind: [u8; 4] = payload[inner..inner + 4].try_into().expect("four bytes");
                    let length = u32::from_le_bytes(
                        payload[inner + 4..inner + 8]
                            .try_into()
                            .expect("four bytes"),
                    ) as usize;
                    let end = (inner + 8)
                        .checked_add(length)
                        .filter(|end| *end <= payload.len())
                        .ok_or_else(|| malformed("truncated frame chunk"))?;
                    if matches!(&kind, b"VP8 " | b"VP8L") {
                        frame_chunk(&kind, &payload[inner + 8..end], None, Some(size))?;
                    }
                    inner = end + (length & 1);
                }
            }
            _ => {}
        }
        at = end + (length as usize & 1);
    }
    Ok(())
}

fn u24(bytes: &[u8]) -> u32 {
    u32::from(bytes[0]) | u32::from(bytes[1]) << 8 | u32::from(bytes[2]) << 16
}

fn within_limits((width, height): (u32, u32)) -> Result<(), ImageProblem> {
    if width > MAX_SIDE || height > MAX_SIDE || u64::from(width) * u64::from(height) > MAX_PIXELS {
        return Err(ImageProblem::TooLarge(format!(
            "{width}×{height} pixels (limit {MAX_SIDE} px per side, {MAX_PIXELS} pixels)"
        )));
    }
    Ok(())
}

/// The size a VP8 or VP8L frame declares, which must be within the limits and equal
/// to the canvas (a still image) or to its animation frame.
fn frame_chunk(
    kind: &[u8; 4],
    payload: &[u8],
    canvas: Option<(u32, u32)>,
    frame: Option<(u32, u32)>,
) -> Result<(), ImageProblem> {
    let malformed = |why: &str| ImageProblem::Decode(format!("WebP: {why}"));
    let size = if kind == b"VP8L" {
        if payload.len() < 5 || payload[0] != 0x2f {
            return Err(malformed("bad VP8L header"));
        }
        let bits = u32::from_le_bytes(payload[1..5].try_into().expect("four bytes"));
        ((bits & 0x3fff) + 1, ((bits >> 14) & 0x3fff) + 1)
    } else {
        if payload.len() < 10 || payload[0] & 1 != 0 || payload[3..6] != [0x9d, 0x01, 0x2a] {
            return Err(malformed("bad VP8 key frame header"));
        }
        let width = u32::from(u16::from_le_bytes([payload[6], payload[7]]) & 0x3fff);
        let height = u32::from(u16::from_le_bytes([payload[8], payload[9]]) & 0x3fff);
        (width, height)
    };
    within_limits(size)?;
    match canvas.or(frame) {
        Some(expected) if expected != size => Err(malformed("frame size differs from the canvas")),
        _ => Ok(()),
    }
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
    let bytes = match media {
        MediaType::Png => {
            stripped = strip_png(bytes)?;
            stripped.as_slice()
        }
        MediaType::Webp => {
            check_webp(bytes)?;
            bytes
        }
        MediaType::Jpeg | MediaType::Gif => bytes,
    };
    let mut reader = image::ImageReader::with_format(Cursor::new(bytes), format(media));
    reader.limits(limits);
    let decoder = reader.into_decoder().map_err(problem)?;
    let (width, height) = decoder.dimensions();
    if u64::from(width) * u64::from(height) > MAX_PIXELS
        || decoder.total_bytes() > MAX_DECODED_BYTES
    {
        return Err(ImageProblem::TooLarge(format!(
            "{width}×{height} pixels (limit {MAX_SIDE} px per side, {MAX_PIXELS} pixels)"
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
        Self::spawn_with(serial, ctx, move || decode(&bytes, media))
    }

    /// The same with the work given (tests hold a decode back with it).
    pub(super) fn spawn_with(
        serial: u64,
        ctx: egui::Context,
        work: impl FnOnce() -> Result<egui::ColorImage, ImageProblem> + Send + 'static,
    ) -> Self {
        let (sender, receiver) = channel();
        let spawned = std::thread::Builder::new()
            .name("artifact-image-decode".to_owned())
            .spawn({
                let sender = sender.clone();
                move || {
                    // An abandoned job has no receiver: the image is dropped here.
                    let _ = sender.send(work());
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
