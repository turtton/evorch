//! Decoding an image artifact under limits of its own.
//!
//! The compressed bytes are already bounded by the data plane
//! (`MAX_IMAGE_BLOB_BYTES`), but a few kilobytes of PNG can declare an image of
//! gigabytes. The size is therefore read from the header and refused before any
//! pixel is decoded, and the decoder gets an allocation limit besides. Decoding
//! runs on a thread of its own (at most one at a time, see the feature); only the
//! texture upload is the GUI thread's.
use std::{
    io::Cursor,
    sync::mpsc::{Receiver, TryRecvError, channel},
};
use workbench_protocol::namespaces::browser::data::MediaType;

/// The pixels of a 4K frame: what the data plane already treats as the largest picture.
pub(super) const MAX_PIXELS: u64 = 3840 * 2160;
/// A texture side of every GPU the desktop runs on.
pub(super) const MAX_SIDE: u32 = 8192;
/// The decoder's own memory, 16-bit samples included.
pub(super) const MAX_DECODED_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, PartialEq, Eq)]
pub(super) enum ImageProblem {
    /// The header declares more than the limits allow (nothing was decoded).
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

/// Decode `bytes` as `media`: the first frame of an animation, as RGBA.
pub(super) fn decode(bytes: &[u8], media: MediaType) -> Result<egui::ColorImage, ImageProblem> {
    use image::ImageDecoder;
    let decode_error = |e: image::ImageError| ImageProblem::Decode(e.to_string());
    let mut decoder = image::ImageReader::with_format(Cursor::new(bytes), format(media))
        .into_decoder()
        .map_err(decode_error)?;
    let (width, height) = decoder.dimensions();
    if width > MAX_SIDE
        || height > MAX_SIDE
        || u64::from(width) * u64::from(height) > MAX_PIXELS
        || decoder.total_bytes() > MAX_DECODED_BYTES
    {
        return Err(ImageProblem::TooLarge(format!(
            "{width}×{height} ピクセル（上限 {MAX_SIDE} 辺・{MAX_PIXELS} ピクセル）"
        )));
    }
    let mut limits = image::Limits::no_limits();
    limits.max_image_width = Some(MAX_SIDE);
    limits.max_image_height = Some(MAX_SIDE);
    limits.max_alloc = Some(2 * MAX_DECODED_BYTES);
    decoder.set_limits(limits).map_err(decode_error)?;
    let rgba = image::DynamicImage::from_decoder(decoder)
        .map_err(decode_error)?
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
    receiver: Receiver<Result<egui::ColorImage, ImageProblem>>,
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
                    let _ = sender.send(decode(&bytes, media));
                    ctx.request_repaint();
                }
            });
        if let Err(error) = spawned {
            let _ = sender.send(Err(ImageProblem::Decode(error.to_string())));
        }
        Self { serial, receiver }
    }

    /// The result if the thread is done. A thread that died without one is a failure.
    pub(super) fn poll(&self) -> Option<Result<egui::ColorImage, ImageProblem>> {
        match self.receiver.try_recv() {
            Ok(result) => Some(result),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                Some(Err(ImageProblem::Decode("decoder stopped".to_owned())))
            }
        }
    }

    /// Wait for the result (tests wait on the thread itself, never on a clock).
    #[cfg(test)]
    pub(super) fn wait(self) -> Result<egui::ColorImage, ImageProblem> {
        self.receiver
            .recv()
            .unwrap_or_else(|_| Err(ImageProblem::Decode("decoder stopped".to_owned())))
    }
}
