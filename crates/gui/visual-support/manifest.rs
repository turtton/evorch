//! What one capture leaves in its output directory: the PNGs and `manifest.json`.

use std::{collections::BTreeMap, path::Path};

use image::RgbaImage;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Bumped when the manifest or the capture conditions change in a way that makes an
/// older capture incomparable.
pub const SCHEMA: u32 = 1;
/// No capture is larger than this in either dimension.
pub const MAX_DIMENSION: u32 = 4096;
/// A PNG file or manifest larger than this is refused before decoding.
pub const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;
pub const MANIFEST_FILE: &str = "manifest.json";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    pub schema: u32,
    pub fixture_sha256: String,
    pub theme: String,
    pub dpi: f32,
    pub font: FontRecord,
    pub environment: EnvironmentRecord,
    pub scenarios: Vec<ScenarioRecord>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FontRecord {
    pub path: String,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnvironmentRecord {
    pub os: String,
    pub arch: String,
    pub wgpu_backend: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScenarioRecord {
    pub id: String,
    pub file: String,
    pub width: u32,
    pub height: u32,
    /// SHA-256 of the decoded RGBA bytes, not of the PNG file.
    pub rgba_sha256: String,
}

pub fn rgba_sha256(image: &RgbaImage) -> String {
    hex(&Sha256::digest(image.as_raw()))
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A scenario id names a file, so it is limited to what a file name may safely hold.
pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

#[derive(Debug)]
pub struct Capture {
    pub manifest: Manifest,
    pub images: BTreeMap<String, RgbaImage>,
}

pub fn write_png(path: &Path, image: &RgbaImage) -> Result<(), String> {
    image
        .save_with_format(path, image::ImageFormat::Png)
        .map_err(|error| format!("cannot write {}: {error}", path.display()))
}

/// Reads a capture directory, refusing a corrupt or oversized one: a missing file,
/// a PNG whose pixels do not match the recorded hash, or a size the manifest does
/// not state.
pub fn load(dir: &Path) -> Result<Capture, String> {
    let manifest_path = dir.join(MANIFEST_FILE);
    let text = read_bounded(&manifest_path)?;
    let manifest: Manifest = serde_json::from_slice(&text).map_err(|error| {
        format!(
            "{} is not a capture manifest: {error}",
            manifest_path.display()
        )
    })?;
    if manifest.schema != SCHEMA {
        return Err(format!(
            "{} has manifest schema {}, this build reads {SCHEMA}",
            manifest_path.display(),
            manifest.schema
        ));
    }
    let mut images = BTreeMap::new();
    for record in &manifest.scenarios {
        if !valid_id(&record.id) || record.file.contains('/') || record.file.contains("..") {
            return Err(format!(
                "manifest names an unsafe scenario or file: {:?}",
                record.id
            ));
        }
        if images.contains_key(&record.id) {
            return Err(format!("manifest lists scenario {} twice", record.id));
        }
        let path = dir.join(&record.file);
        let bytes = read_bounded(&path)?;
        let image = image::load_from_memory_with_format(&bytes, image::ImageFormat::Png)
            .map_err(|error| format!("{} is not a readable PNG: {error}", path.display()))?
            .to_rgba8();
        if image.width() > MAX_DIMENSION || image.height() > MAX_DIMENSION {
            return Err(format!(
                "{} is larger than {MAX_DIMENSION} px",
                path.display()
            ));
        }
        if (image.width(), image.height()) != (record.width, record.height) {
            return Err(format!(
                "{} is {}x{}, the manifest says {}x{}",
                path.display(),
                image.width(),
                image.height(),
                record.width,
                record.height
            ));
        }
        if rgba_sha256(&image) != record.rgba_sha256 {
            return Err(format!(
                "{} does not match its recorded pixel hash",
                path.display()
            ));
        }
        images.insert(record.id.clone(), image);
    }
    Ok(Capture { manifest, images })
}

fn read_bounded(path: &Path) -> Result<Vec<u8>, String> {
    let meta = std::fs::metadata(path)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    if meta.len() > MAX_FILE_BYTES {
        return Err(format!(
            "{} is larger than {MAX_FILE_BYTES} bytes",
            path.display()
        ));
    }
    std::fs::read(path).map_err(|error| format!("cannot read {}: {error}", path.display()))
}
