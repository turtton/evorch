//! Image formats that tool results can return to a model.

use std::path::Path;

use base64::Engine;

use crate::result::ToolImage;

/// Largest image sent to a model; its base64 form stays within the 5 MiB
/// per-image limit of the strictest provider.
pub const MAX_IMAGE_BYTES: usize = 3_932_160;
/// Longest side providers accept without rejecting the request.
pub const MAX_IMAGE_SIDE: u32 = 8000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageFormat {
    Png,
    Jpeg,
    Gif,
    Webp,
}

impl ImageFormat {
    pub fn from_extension(path: &Path) -> Option<Self> {
        let extension = path.extension()?.to_str()?.to_ascii_lowercase();
        Some(match extension.as_str() {
            "png" => Self::Png,
            "jpg" | "jpeg" => Self::Jpeg,
            "gif" => Self::Gif,
            "webp" => Self::Webp,
            _ => return None,
        })
    }

    pub fn from_media_type(media_type: &str) -> Option<Self> {
        Some(match media_type.to_ascii_lowercase().as_str() {
            "image/png" => Self::Png,
            "image/jpeg" | "image/jpg" => Self::Jpeg,
            "image/gif" => Self::Gif,
            "image/webp" => Self::Webp,
            _ => return None,
        })
    }

    pub const fn media_type(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::Gif => "image/gif",
            Self::Webp => "image/webp",
        }
    }

    pub const fn extension(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpg",
            Self::Gif => "gif",
            Self::Webp => "webp",
        }
    }

    /// Whether the bytes carry this format's signature, so a mislabeled file
    /// is never sent as an image.
    pub fn matches(self, bytes: &[u8]) -> bool {
        match self {
            Self::Png => bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
            Self::Jpeg => bytes.starts_with(&[0xFF, 0xD8, 0xFF]),
            Self::Gif => bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a"),
            Self::Webp => bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP",
        }
    }

    const fn decoder_format(self) -> image::ImageFormat {
        match self {
            Self::Png => image::ImageFormat::Png,
            Self::Jpeg => image::ImageFormat::Jpeg,
            Self::Gif => image::ImageFormat::Gif,
            Self::Webp => image::ImageFormat::WebP,
        }
    }
}

/// An image that passed the checks for being sent to a model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckedImage {
    pub format: ImageFormat,
    pub width: u32,
    pub height: u32,
    pub bytes: usize,
    data: String,
}

impl CheckedImage {
    pub fn into_tool_image(self, path: Option<std::path::PathBuf>) -> ToolImage {
        ToolImage {
            media_type: self.format.media_type().to_owned(),
            data: self.data,
            path,
        }
    }

    pub fn summary(&self) -> String {
        format!(
            "{}, {}x{}, {} bytes",
            self.format.media_type(),
            self.width,
            self.height,
            self.bytes
        )
    }
}

/// Validate signature, size and dimensions without decoding pixel data.
pub fn check(format: ImageFormat, bytes: &[u8]) -> Result<CheckedImage, String> {
    if !format.matches(bytes) {
        return Err(format!("content is not a valid {}", format.media_type()));
    }
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err(format!(
            "image is {} bytes; the limit is {MAX_IMAGE_BYTES} bytes. Save a smaller or downscaled copy and read that instead",
            bytes.len()
        ));
    }
    let (width, height) =
        image::ImageReader::with_format(std::io::Cursor::new(bytes), format.decoder_format())
            .into_dimensions()
            .map_err(|error| format!("cannot read image dimensions: {error}"))?;
    if width.max(height) > MAX_IMAGE_SIDE {
        return Err(format!(
            "image is {width}x{height}; the longest side must be at most {MAX_IMAGE_SIDE} pixels"
        ));
    }
    Ok(CheckedImage {
        format,
        width,
        height,
        bytes: bytes.len(),
        data: base64::engine::general_purpose::STANDARD.encode(bytes),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = Vec::new();
        image::RgbaImage::new(width, height)
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Png,
            )
            .unwrap();
        bytes
    }

    #[test]
    fn valid_png_is_encoded_with_its_dimensions() {
        let bytes = png(3, 2);
        let checked = check(ImageFormat::Png, &bytes).unwrap();
        assert_eq!((checked.width, checked.height), (3, 2));
        let image = checked.into_tool_image(None);
        assert_eq!(image.media_type, "image/png");
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(image.data)
                .unwrap(),
            bytes
        );
    }

    #[test]
    fn mislabeled_and_oversized_images_are_rejected() {
        assert!(check(ImageFormat::Jpeg, &png(1, 1)).is_err());
        assert!(check(ImageFormat::Png, b"not an image").is_err());
        let error = check(ImageFormat::Png, &png(MAX_IMAGE_SIDE + 1, 1)).unwrap_err();
        assert!(error.contains("longest side"), "{error}");
    }
}
