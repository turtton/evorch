//! Exact RGBA comparison of two captures. No threshold: noise is fixed at the source
//! (the fixture, the fonts, the software renderer), never hidden here.

use std::{collections::BTreeSet, path::Path};

use image::{Rgba, RgbaImage};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::manifest::{Capture, hex, write_png};

pub const REPORT_FILE: &str = "report.json";
pub const REPORT_SCHEMA: u32 = 1;
const HIGHLIGHT: Rgba<u8> = Rgba([255, 0, 90, 255]);
const BOX_PADDING: u32 = 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Unchanged,
    Changed,
    /// Only the after capture has it (also every scenario when there is no baseline).
    Added,
    /// Only the before capture has it.
    Removed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub id: String,
    pub status: Status,
    pub changed_pixels: u64,
    /// `[left, top, right, bottom)` of the changed pixels.
    pub bounds: Option<[u32; 4]>,
    /// Published file names; present only for what the status shows.
    pub before: Option<String>,
    pub after: Option<String>,
    pub diff: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Report {
    pub schema: u32,
    /// False when the before side has no capture tool at all ("no baseline").
    pub baseline: bool,
    /// The fixture source differs between the two sides: an intended change of what is drawn.
    pub fixture_changed: bool,
    pub entries: Vec<Entry>,
}

impl Report {
    pub fn shown(&self) -> impl Iterator<Item = &Entry> {
        self.entries
            .iter()
            .filter(|e| e.status != Status::Unchanged)
    }

    pub fn has_difference(&self) -> bool {
        self.shown().next().is_some()
    }
}

/// Two captures made for the comparison must be taken under the same conditions.
fn check_comparable(before: &Capture, after: &Capture) -> Result<(), String> {
    let (b, a) = (&before.manifest, &after.manifest);
    if b.schema != a.schema {
        return Err(format!("manifest schema {} against {}", b.schema, a.schema));
    }
    if b.theme != a.theme || (b.dpi - a.dpi).abs() > f32::EPSILON {
        return Err(format!(
            "capture conditions differ: {} at {} dpi against {} at {} dpi",
            b.theme, b.dpi, a.theme, a.dpi
        ));
    }
    if b.font != a.font {
        return Err(format!(
            "the CJK font differs ({} {} against {} {}): the sides were not drawn alike",
            b.font.path, b.font.sha256, a.font.path, a.font.sha256
        ));
    }
    if b.environment != a.environment {
        return Err(format!(
            "the drawing environments differ: {:?} against {:?}",
            b.environment, a.environment
        ));
    }
    for (id, before_image) in &before.images {
        if let Some(after_image) = after.images.get(id)
            && before_image.dimensions() != after_image.dimensions()
        {
            return Err(format!(
                "scenario {id} is {:?} before and {:?} after: not comparable",
                before_image.dimensions(),
                after_image.dimensions()
            ));
        }
    }
    Ok(())
}

/// Demands that two captures of the same code agree to the last pixel (the repeat
/// check of every capture job).
pub fn require_identical(first: &Capture, second: &Capture) -> Result<(), String> {
    check_comparable(first, second)?;
    let ids: BTreeSet<_> = first.images.keys().chain(second.images.keys()).collect();
    let mut problems = Vec::new();
    for id in ids {
        match (first.images.get(id), second.images.get(id)) {
            (Some(a), Some(b)) => {
                let (changed, ..) = diff_pixels(a, b);
                if changed != 0 {
                    problems.push(format!("{id}: {changed} pixels differ"));
                }
            }
            _ => problems.push(format!("{id}: present in only one capture")),
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "the repeated capture is not deterministic: {}",
            problems.join("; ")
        ))
    }
}

/// Compares `after` with `before` (`None`: the baseline has no capture tool) and writes the
/// images a comment shows into `out`.
pub fn compare(before: Option<&Capture>, after: &Capture, out: &Path) -> Result<Report, String> {
    if let Some(before) = before {
        check_comparable(before, after)?;
    }
    std::fs::create_dir_all(out)
        .map_err(|error| format!("cannot create {}: {error}", out.display()))?;
    let ids: BTreeSet<&String> = after
        .images
        .keys()
        .chain(before.iter().flat_map(|b| b.images.keys()))
        .collect();
    let mut entries = Vec::new();
    for id in ids {
        let old = before.and_then(|b| b.images.get(id));
        let new = after.images.get(id);
        let entry = match (old, new) {
            (Some(old), Some(new)) => {
                let (changed, bounds, diff) = diff_pixels(old, new);
                if changed == 0 {
                    Entry {
                        id: id.clone(),
                        status: Status::Unchanged,
                        changed_pixels: 0,
                        bounds: None,
                        before: None,
                        after: None,
                        diff: None,
                    }
                } else {
                    Entry {
                        id: id.clone(),
                        status: Status::Changed,
                        changed_pixels: changed,
                        bounds,
                        before: Some(publish(out, id, "before", old)?),
                        after: Some(publish(out, id, "after", new)?),
                        diff: Some(publish(out, id, "diff", &diff)?),
                    }
                }
            }
            (None, Some(new)) => Entry {
                id: id.clone(),
                status: Status::Added,
                changed_pixels: 0,
                bounds: None,
                before: None,
                after: Some(publish(out, id, "after", new)?),
                diff: None,
            },
            (Some(old), None) => Entry {
                id: id.clone(),
                status: Status::Removed,
                changed_pixels: 0,
                bounds: None,
                before: Some(publish(out, id, "before", old)?),
                after: None,
                diff: None,
            },
            (None, None) => unreachable!("the id comes from one of the captures"),
        };
        entries.push(entry);
    }
    let report = Report {
        schema: REPORT_SCHEMA,
        baseline: before.is_some(),
        fixture_changed: before
            .is_some_and(|b| b.manifest.fixture_sha256 != after.manifest.fixture_sha256),
        entries,
    };
    let text = serde_json::to_string_pretty(&report).map_err(|error| error.to_string())?;
    std::fs::write(out.join(REPORT_FILE), text)
        .map_err(|error| format!("cannot write the report: {error}"))?;
    Ok(report)
}

/// Writes `<id>-<role>-<first 12 hex of the file's sha256>.png`: the content is part of the
/// name, so a changed image is a new URL.
fn publish(out: &Path, id: &str, role: &str, image: &RgbaImage) -> Result<String, String> {
    let temporary = out.join(format!(".{id}-{role}.png"));
    write_png(&temporary, image)?;
    let bytes = std::fs::read(&temporary).map_err(|error| error.to_string())?;
    let name = format!("{id}-{role}-{}.png", &hex(&Sha256::digest(&bytes))[..12]);
    std::fs::rename(&temporary, out.join(&name)).map_err(|error| error.to_string())?;
    Ok(name)
}

/// The number of differing pixels, their bounding box, and the highlight image: the after
/// capture faded, the changed pixels in a strong colour and the area boxed.
pub fn diff_pixels(before: &RgbaImage, after: &RgbaImage) -> (u64, Option<[u32; 4]>, RgbaImage) {
    let (width, height) = after.dimensions();
    let mut diff = RgbaImage::new(width, height);
    let mut changed = 0u64;
    let mut bounds: Option<[u32; 4]> = None;
    for (x, y, pixel) in after.enumerate_pixels() {
        let differs = before.get_pixel_checked(x, y) != Some(pixel);
        if differs {
            changed += 1;
            diff.put_pixel(x, y, HIGHLIGHT);
            bounds = Some(match bounds {
                None => [x, y, x + 1, y + 1],
                Some([l, t, r, b]) => [l.min(x), t.min(y), r.max(x + 1), b.max(y + 1)],
            });
        } else {
            let fade = |c: u8| 255 - (255 - c) / 4;
            diff.put_pixel(
                x,
                y,
                Rgba([fade(pixel[0]), fade(pixel[1]), fade(pixel[2]), 255]),
            );
        }
    }
    if let Some([l, t, r, b]) = bounds {
        draw_box(
            &mut diff,
            l.saturating_sub(BOX_PADDING),
            t.saturating_sub(BOX_PADDING),
            (r + BOX_PADDING).min(width),
            (b + BOX_PADDING).min(height),
        );
    }
    (changed, bounds, diff)
}

fn draw_box(image: &mut RgbaImage, left: u32, top: u32, right: u32, bottom: u32) {
    for x in left..right {
        image.put_pixel(x, top, HIGHLIGHT);
        image.put_pixel(x, bottom - 1, HIGHLIGHT);
    }
    for y in top..bottom {
        image.put_pixel(left, y, HIGHLIGHT);
        image.put_pixel(right - 1, y, HIGHLIGHT);
    }
}

pub fn read_report(dir: &Path) -> Result<Report, String> {
    let text = std::fs::read(dir.join(REPORT_FILE)).map_err(|error| error.to_string())?;
    serde_json::from_slice(&text).map_err(|error| error.to_string())
}
