use super::{Variant, VariantFormat};
use base64::Engine;
use image::{DynamicImage, ImageFormat, ImageReader};
use std::io::Cursor;
use std::path::PathBuf;

fn decode(bytes: &[u8], format: ImageFormat) -> Result<DynamicImage, String> {
    let reader = ImageReader::with_format(Cursor::new(bytes), format);
    // Keep decoder allocation limits; even tiny compressed inputs can expand hugely.
    reader
        .decode()
        .map_err(|_| "image cannot be decoded within allocation limits".into())
}

fn encode(image: &DynamicImage, format: VariantFormat) -> Result<Vec<u8>, String> {
    let mut output = Cursor::new(Vec::new());
    match format {
        VariantFormat::Png => image.write_to(&mut output, ImageFormat::Png),
        VariantFormat::Jpeg => image::codecs::jpeg::JpegEncoder::new_with_quality(&mut output, 85)
            .encode_image(&image.to_rgb8()),
    }
    .map_err(|_| "cannot encode resized image")?;
    Ok(output.into_inner())
}

pub(super) fn references(
    roots: &crate::artifacts::SourceRoots,
    paths: &[PathBuf],
) -> Result<Vec<providers::InlineImage>, String> {
    paths
        .iter()
        .map(|path| {
            let (_, format, bytes) = roots.read_image(path)?;
            let decoder_format = match format {
                tools::image::ImageFormat::Png => ImageFormat::Png,
                tools::image::ImageFormat::Jpeg => ImageFormat::Jpeg,
                tools::image::ImageFormat::Gif => ImageFormat::Gif,
                tools::image::ImageFormat::Webp => ImageFormat::WebP,
            };
            let image = decode(&bytes, decoder_format)?;
            let (bytes, media_type) = if image.width().max(image.height()) > 2048 {
                (
                    encode(&image.thumbnail(2048, 2048), VariantFormat::Png)?,
                    "image/png",
                )
            } else {
                (bytes, format.media_type())
            };
            Ok(providers::InlineImage {
                media_type: media_type.into(),
                data: base64::engine::general_purpose::STANDARD.encode(bytes),
            })
        })
        .collect()
}

pub(super) struct Outputs {
    pub width: u32,
    pub height: u32,
    pub variant: Option<Vec<u8>>,
    pub preview: String,
}

fn thumbnail(image: &DynamicImage, side: u32) -> DynamicImage {
    if image.width().max(image.height()) <= side {
        image.clone()
    } else {
        image.thumbnail(side, side)
    }
}

pub(super) fn outputs(png: &[u8], variant: Option<&Variant>) -> Result<Outputs, String> {
    let image = decode(png, ImageFormat::Png)?;
    let variant = variant
        .map(|v| encode(&thumbnail(&image, v.max_side), v.format))
        .transpose()?;
    let mut side = 1024;
    let preview = loop {
        let bytes = encode(&thumbnail(&image, side), VariantFormat::Png)?;
        if bytes.len() <= tools::image::MAX_IMAGE_BYTES {
            break bytes;
        }
        side /= 2;
    };
    Ok(Outputs {
        width: image.width(),
        height: image.height(),
        variant,
        preview: base64::engine::general_purpose::STANDARD.encode(preview),
    })
}
