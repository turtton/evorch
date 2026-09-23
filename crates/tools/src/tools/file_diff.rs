//! Unified text diff returned by file-mutating tools to the agent and GUI.

use std::path::Path;

use similar::TextDiff;

const MAX_DIFF_INPUT_BYTES: usize = 2 * 1024 * 1024;

pub(super) fn read_previous(path: &Path) -> Result<Option<String>, String> {
    if let Ok(metadata) = std::fs::metadata(path)
        && metadata.len() > MAX_DIFF_INPUT_BYTES as u64
    {
        return Err(format!(
            "previous file exceeds {MAX_DIFF_INPUT_BYTES} bytes"
        ));
    }
    match std::fs::read_to_string(path) {
        Ok(content) => Ok(Some(content)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("could not read previous contents: {error}")),
    }
}

pub(super) fn changed_file(path: &str, before: Option<&str>, after: &str) -> String {
    if before == Some(after) {
        return format!("No changes to {path}");
    }
    if before.is_none() && after.is_empty() {
        return format!("Created empty file {path}");
    }
    if before.is_some_and(|content| content.len() > MAX_DIFF_INPUT_BYTES)
        || after.len() > MAX_DIFF_INPUT_BYTES
    {
        return format!(
            "Changed {path}; diff omitted because the file exceeds {MAX_DIFF_INPUT_BYTES} bytes"
        );
    }

    let old_label = before.map_or_else(|| "/dev/null".to_owned(), |_| format!("a/{path}"));
    let new_label = format!("b/{path}");
    TextDiff::from_lines(before.unwrap_or(""), after)
        .unified_diff()
        .header(&old_label, &new_label)
        .to_string()
}
