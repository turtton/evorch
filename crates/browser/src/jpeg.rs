use base64::{Engine, engine::general_purpose::STANDARD};

#[derive(Debug, thiserror::Error)]
pub(crate) enum FrameError {
    #[error(transparent)]
    Image(#[from] image::ImageError),
    #[error(transparent)]
    Base64(#[from] base64::DecodeError),
}

/// Decodes one base64 JPEG screencast frame, refusing frames larger than
/// `max` or allocations over 16 MiB.
pub(crate) fn decode_jpeg(
    data: impl AsRef<[u8]>,
    max: [u32; 2],
) -> Result<image::RgbaImage, FrameError> {
    let jpeg = STANDARD.decode(data)?;
    let mut reader =
        image::ImageReader::with_format(std::io::Cursor::new(jpeg), image::ImageFormat::Jpeg);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(max[0]);
    limits.max_image_height = Some(max[1]);
    limits.max_alloc = Some(16 * 1024 * 1024);
    reader.limits(limits);
    Ok(reader.decode()?.into_rgba8())
}
