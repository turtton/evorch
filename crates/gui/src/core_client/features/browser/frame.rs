//! Pixels: a raw BGRA frame to a texture, and a Capture PNG to a bounded image.
//!
//! Receiving is the shared connection's business; this runs on the GUI thread
//! at render time (a frame is converted and uploaded once, at most one frame
//! per tab per render) except for the PNG decode, which runs on a helper
//! thread (a few at most) and hands the image back through a channel the
//! render polls.
use std::{
    io::Cursor,
    sync::{Arc, Condvar, Mutex, mpsc},
};

use egui::{Color32, ColorImage, Context, TextureHandle, TextureOptions};
use image::{ImageReader, Limits};
use workbench_client::renderer::Frame;

/// The largest image either side shows, the renderer's own viewport bound.
pub(super) const MAX_WIDTH: u32 = 3840;
pub(super) const MAX_HEIGHT: u32 = 2160;
/// Memory the decoder may allocate for one Capture (RGBA of the largest image).
const MAX_DECODE_BYTES: u64 = MAX_WIDTH as u64 * MAX_HEIGHT as u64 * 4;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum PixelError {
    Size,
    Stride,
    Short,
    Decode(String),
}

impl PixelError {
    pub fn describe(&self) -> String {
        match self {
            Self::Size => "image size is outside the supported range".to_owned(),
            Self::Stride => "frame stride is smaller than its row".to_owned(),
            Self::Short => "frame is shorter than its size".to_owned(),
            Self::Decode(error) => format!("image could not be decoded: {error}"),
        }
    }
}

/// A frame of BGRA8 in `stride`-byte rows as an image. The rows are copied
/// without their padding, so a stride wider than the row is accepted.
pub(super) fn bgra_to_image(frame: &Frame) -> Result<ColorImage, PixelError> {
    let (w, h) = (frame.start.width, frame.start.height);
    if w == 0 || h == 0 || w > MAX_WIDTH || h > MAX_HEIGHT {
        return Err(PixelError::Size);
    }
    let (w, h, stride) = (w as usize, h as usize, frame.start.stride as usize);
    let row = w * 4;
    if stride < row {
        return Err(PixelError::Stride);
    }
    // The last row needs no padding after it.
    if frame.bgra.len() < stride * (h - 1) + row {
        return Err(PixelError::Short);
    }
    let mut pixels = Vec::with_capacity(w * h);
    for line in 0..h {
        let start = line * stride;
        pixels.extend(
            frame.bgra[start..start + row]
                .as_chunks::<4>()
                .0
                .iter()
                .map(|px| Color32::from_rgba_premultiplied(px[2], px[1], px[0], px[3])),
        );
    }
    Ok(ColorImage::new([w, h], pixels))
}

/// A decoded Capture: dimensions are read from the header and checked before
/// the decoder allocates anything.
pub(super) fn decode_capture(bytes: &[u8]) -> Result<ColorImage, PixelError> {
    let mut reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| PixelError::Decode(e.to_string()))?;
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_WIDTH);
    limits.max_image_height = Some(MAX_HEIGHT);
    limits.max_alloc = Some(MAX_DECODE_BYTES);
    reader.limits(limits);
    let decoded = reader
        .decode()
        .map_err(|e| PixelError::Decode(e.to_string()))?;
    let rgba = decoded.to_rgba8();
    let size = [rgba.width() as usize, rgba.height() as usize];
    if size[0] == 0 || size[1] == 0 {
        return Err(PixelError::Size);
    }
    Ok(ColorImage::from_rgba_unmultiplied(size, rgba.as_raw()))
}

/// Decoder threads that may be alive at once. A decode cannot be interrupted,
/// so a thread counts until it really ends, also when its job was dropped.
pub(super) const MAX_DECODERS: usize = 2;

/// How many decoder threads are alive. Cloned into each thread; the count is
/// released when the thread ends.
#[derive(Clone, Default)]
pub(super) struct Decoders(Arc<(Mutex<usize>, Condvar)>);

struct Permit(Decoders);

impl Decoders {
    fn acquire(&self) -> Option<Permit> {
        let mut active = self.0.0.lock().ok()?;
        if *active >= MAX_DECODERS {
            return None;
        }
        *active += 1;
        Some(Permit(self.clone()))
    }

    /// Blocks until at most `n` decoder threads are alive (tests: the signal
    /// that a thread ended).
    #[cfg(test)]
    pub fn wait_at_most(&self, n: usize) {
        let guard = self.0.0.lock().unwrap();
        drop(self.0.1.wait_while(guard, |active| *active > n).unwrap());
    }

    #[cfg(test)]
    pub fn active(&self) -> usize {
        *self.0.0.lock().unwrap()
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        if let Ok(mut active) = (self.0).0.0.lock() {
            *active -= 1;
        }
        (self.0).0.1.notify_all();
    }
}

/// One decode in the background. Dropping it discards the result; the thread
/// still counts against [`MAX_DECODERS`] until it ends.
pub(super) struct DecodeJob {
    result: mpsc::Receiver<Result<ColorImage, PixelError>>,
    /// A result a test took off the channel to know the decode finished.
    #[cfg(test)]
    waited: Mutex<Option<Result<ColorImage, PixelError>>>,
}

impl DecodeJob {
    /// `None` when every decoder is busy or the thread cannot start; the
    /// bytes are dropped then.
    pub fn start(bytes: Vec<u8>, repaint: Context, decoders: &Decoders) -> Option<Self> {
        Self::start_with(bytes, repaint, decoders, decode_capture)
    }

    pub fn start_with(
        bytes: Vec<u8>,
        repaint: Context,
        decoders: &Decoders,
        decode: impl FnOnce(&[u8]) -> Result<ColorImage, PixelError> + Send + 'static,
    ) -> Option<Self> {
        let permit = decoders.acquire()?;
        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("browser-capture-decode".into())
            .spawn(move || {
                let result = decode(&bytes);
                drop(bytes);
                // A dropped job has no receiver; the image is discarded here.
                if tx.send(result).is_ok() {
                    repaint.request_repaint();
                }
                drop(permit);
            })
            .ok()?;
        Some(Self {
            result: rx,
            #[cfg(test)]
            waited: Mutex::default(),
        })
    }

    /// The result if the decode finished. A decode that could not even start
    /// reports as a failure.
    pub fn poll(&self) -> Option<Result<ColorImage, PixelError>> {
        #[cfg(test)]
        if let Some(result) = self.waited.lock().unwrap().take() {
            return Some(result);
        }
        match self.result.try_recv() {
            Ok(result) => Some(result),
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => {
                Some(Err(PixelError::Decode("decoder stopped".to_owned())))
            }
        }
    }

    /// Blocks until the decode finished (tests: an explicit completion signal);
    /// the result is then returned by the next `poll`.
    #[cfg(test)]
    pub fn wait_ready(&self) {
        let result = self
            .result
            .recv()
            .unwrap_or_else(|_| Err(PixelError::Decode("decoder stopped".to_owned())));
        *self.waited.lock().unwrap() = Some(result);
    }
}

/// The texture of the picture on screen. It is uploaded from the GUI thread
/// only, replaced when the size changes and dropped on `clear`.
#[derive(Default)]
pub(super) struct Picture {
    texture: Option<TextureHandle>,
    /// Sequence of the frame on screen, for frames.
    pub sequence: Option<u64>,
}

impl Picture {
    pub fn show(&mut self, ctx: &Context, name: &str, image: ColorImage, sequence: Option<u64>) {
        self.sequence = sequence;
        match &mut self.texture {
            Some(texture) => texture.set(image, TextureOptions::LINEAR),
            None => self.texture = Some(ctx.load_texture(name, image, TextureOptions::LINEAR)),
        }
    }

    pub fn clear(&mut self) {
        self.texture = None;
        self.sequence = None;
    }

    pub fn texture(&self) -> Option<&TextureHandle> {
        self.texture.as_ref()
    }
}
