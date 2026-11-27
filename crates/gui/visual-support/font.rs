//! The CJK font of a capture. The coordinator points `FONTCONFIG_FILE` at a
//! configuration that knows exactly one font file; the GUI resolves it with the same
//! `fc-match` query it always uses (`theme::fonts::FONT_QUERY`). Here the same query is
//! run first and its answer must be that file, readable, with the hash the manifest
//! records. A missing or different font fails the capture instead of drawing tofu.

use std::process::Command;

use sha2::{Digest, Sha256};

use super::manifest::{FontRecord, hex};
use gui::theme::fonts::FONT_QUERY;

/// What `fc-match` prints for the query, from this process's environment.
pub fn resolve_path(output: &std::process::Output) -> Result<String, String> {
    let path = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if !output.status.success() || path.is_empty() {
        return Err(format!(
            "fc-match {FONT_QUERY} found no font ({})",
            output.status
        ));
    }
    Ok(path)
}

/// Checks that the query resolves to `expected` and returns its record.
pub fn pinned(expected: &str) -> Result<FontRecord, String> {
    let output = Command::new("fc-match")
        .args(["--format=%{file}", FONT_QUERY])
        .output()
        .map_err(|error| format!("cannot run fc-match: {error}"))?;
    let path = resolve_path(&output)?;
    if path != expected {
        return Err(format!(
            "fontconfig resolves {FONT_QUERY} to {path}, expected {expected}"
        ));
    }
    let bytes =
        std::fs::read(&path).map_err(|error| format!("cannot read the font {path}: {error}"))?;
    Ok(FontRecord {
        path,
        sha256: hex(&Sha256::digest(&bytes)),
    })
}
