//! Bounded tool output and temporary, redacted artifacts.
//!
//! A fixed number of independently locked slots bounds disk use across restarts
//! and processes. Replacing a slot removes at most one bounded file. Paths carry
//! unique IDs, so an expired reference never silently reads another command's log.
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime};

use crate::ToolResult;
use secret_guard::SecretRedactor;

pub const PREVIEW_BYTES: usize = 16 * 1024;
pub const PREVIEW_LINES: usize = 300;
pub const CAPTURE_BYTES: usize = 8 * 1024 * 1024;
const SLOTS: usize = 64;
const TTL: Duration = Duration::from_secs(7 * 24 * 3600);
const MARKER: &str = "\n[Output artifact: ";

/// Keeps an initial bounded artifact and a bounded tail while draining all bytes.
#[derive(Debug, Default)]
pub(crate) struct Capture {
    pub(crate) bytes: u64,
    prefix: Vec<u8>,
    tail: Vec<u8>,
}
impl Capture {
    pub(crate) fn push(&mut self, bytes: &[u8]) {
        self.bytes = self.bytes.saturating_add(bytes.len() as u64);
        let available = CAPTURE_BYTES.saturating_sub(self.prefix.len());
        self.prefix
            .extend_from_slice(&bytes[..bytes.len().min(available)]);
        if bytes.len() >= PREVIEW_BYTES {
            self.tail.clear();
            self.tail
                .extend_from_slice(&bytes[bytes.len() - PREVIEW_BYTES..]);
        } else {
            let excess = (self.tail.len() + bytes.len()).saturating_sub(PREVIEW_BYTES);
            self.tail.drain(..excess);
            self.tail.extend_from_slice(bytes);
        }
    }
    pub(crate) fn append(&mut self, other: &Self) {
        if other.bytes == 0 {
            return;
        }
        if self.bytes > 0 && self.tail.last() != Some(&b'\n') {
            self.push(b"\n");
        }
        let before = self.bytes;
        self.push(&other.prefix);
        self.bytes = before.saturating_add(other.bytes);
        if other.bytes > other.prefix.len() as u64 {
            self.tail.clone_from(&other.tail);
        }
    }
    pub(crate) fn finish(self) -> ToolResult {
        let text = String::from_utf8_lossy(&self.prefix);
        let truncated = self.bytes > self.prefix.len() as u64;
        let large = truncated || text.len() > PREVIEW_BYTES || text.lines().count() > PREVIEW_LINES;
        if !large {
            return ToolResult::success(text.into_owned());
        }
        let redactor = SecretRedactor::from_env();
        let safe = redactor.redact(&text);
        artifact_result(&safe.text, &safe.text, self.bytes, !truncated, safe.count)
    }
}

/// Apply the same ceiling before any event, GUI, or model receives a tool result.
pub(crate) fn limit_result(mut result: ToolResult) -> ToolResult {
    if result.content.len() > PREVIEW_BYTES + 2048
        || result.content.lines().count() > PREVIEW_LINES + 8
    {
        let mut capture = Capture::default();
        capture.push(result.content.as_bytes());
        let limited = capture.finish();
        result.content = limited.content;
        result.detail = limited.detail;
    }
    // Structured output from external tools must not bypass the text ceiling.
    if result
        .detail
        .as_ref()
        .is_some_and(|value| value.to_string().len() > PREVIEW_BYTES)
    {
        result.detail =
            Some(serde_json::json!({"detail_omitted": "structured output exceeded 16 KiB"}));
    }
    result
}

fn artifact_result(
    text: &str,
    tail: &str,
    original_bytes: u64,
    complete: bool,
    redactions: usize,
) -> ToolResult {
    let saved = output_root().and_then(|root| store().save(&root, text));
    let preview = tail_preview(tail);
    let (notice, path, saved_bytes, complete) = match saved {
        Ok((path, bytes)) => {
            let complete = complete && bytes == text.len();
            let status = if complete {
                "complete"
            } else {
                "prefix only: capture limit reached; preview is the end of the captured prefix"
            };
            (
                format!(
                    "{MARKER}{}; captured {bytes} of {original_bytes} bytes; {status}; redacted spans: {redactions}. Use read with offset/limit or grep. Temporary logs can expire.]",
                    path.display()
                ),
                Some(path),
                bytes,
                complete,
            )
        }
        Err(error) => (
            format!(
                "\n[Output truncated; {original_bytes} bytes produced. Artifact unavailable: {error}]"
            ),
            None,
            0,
            false,
        ),
    };
    ToolResult::success(format!("{preview}{notice}")).with_detail(serde_json::json!({
        "output_artifact": {"path": path, "original_bytes": original_bytes, "saved_bytes": saved_bytes, "complete": complete, "redactions": redactions}
    }))
}

fn tail_preview(text: &str) -> &str {
    let mut start = text.len().saturating_sub(PREVIEW_BYTES);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    let text = &text[start..];
    let mut count = 0;
    for (index, ch) in text.char_indices().rev() {
        if ch == '\n' {
            count += 1;
        }
        if count > PREVIEW_LINES {
            return &text[index + 1..];
        }
    }
    text
}

/// Save older in-memory text before removing its preview from model context.
pub fn archive_text(text: &str) -> Option<String> {
    let safe = SecretRedactor::from_env().redact(text);
    let result = artifact_result(
        &safe.text,
        "",
        text.len() as u64,
        text.len() <= CAPTURE_BYTES,
        safe.count,
    );
    artifact_reference(&result.content).map(str::to_owned)
}

/// Extract a compact, self-contained reference for old tool-result pruning.
pub fn artifact_reference(text: &str) -> Option<&str> {
    let index = text.rfind(MARKER)?;
    text.ends_with(']').then_some(&text[index + 1..])
}

struct OutputStore {
    next: AtomicUsize,
}
fn store() -> &'static OutputStore {
    static STORE: OnceLock<OutputStore> = OnceLock::new();
    STORE.get_or_init(|| OutputStore {
        next: AtomicUsize::new(0),
    })
}

/// Host path mounted read-only into the command sandbox. Never uses TMPDIR.
pub fn output_root() -> io::Result<PathBuf> {
    let root = PathBuf::from("/var/tmp").join(format!(
        "evorch-output-{}",
        rustix::process::geteuid().as_raw()
    ));
    private_dir(&root)?;
    start_cleanup(root.clone());
    Ok(root)
}
fn private_dir(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    let mut builder = std::fs::DirBuilder::new();
    builder.mode(0o700);
    match builder.create(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    let meta = std::fs::symlink_metadata(path)?;
    if !meta.is_dir()
        || meta.uid() != rustix::process::geteuid().as_raw()
        || meta.mode() & 0o077 != 0
    {
        return Err(io::Error::other(
            "output directory must be private and owned by the current user",
        ));
    }
    Ok(())
}
fn locked_slot(root: &Path, index: usize) -> io::Result<(PathBuf, File)> {
    use std::os::unix::fs::OpenOptionsExt;
    let dir = root.join(index.to_string());
    private_dir(&dir)?;
    let lock = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
        .open(dir.join(".lock"))?;
    lock.try_lock().map_err(io::Error::other)?;
    Ok((dir, lock))
}
impl OutputStore {
    fn save(&self, root: &Path, text: &str) -> io::Result<(PathBuf, usize)> {
        use std::os::unix::fs::OpenOptionsExt;
        let first = self.next.fetch_add(1, Ordering::Relaxed) % SLOTS;
        for offset in 0..SLOTS {
            let Ok((dir, _lock)) = locked_slot(root, (first + offset) % SLOTS) else {
                continue;
            };
            // Each slot contains one artifact and its lock; no recursive deletion.
            for entry in std::fs::read_dir(&dir)?.take(3) {
                let entry = entry?;
                if entry.file_name() != ".lock" {
                    std::fs::remove_file(entry.path())?;
                }
            }
            let path = dir.join(format!("{}.txt", uuid::Uuid::new_v4()));
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)?;
            let mut end = text.len().min(CAPTURE_BYTES);
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            file.write_all(&text.as_bytes()[..end])?;
            return Ok((path, end));
        }
        Err(io::Error::other("all temporary output slots are busy"))
    }
}
fn start_cleanup(root: PathBuf) {
    static STARTED: OnceLock<()> = OnceLock::new();
    STARTED.get_or_init(|| {
        let _ = std::thread::Builder::new()
            .name("output-cleanup".into())
            .spawn(move || {
                let mut slot = 0;
                loop {
                    std::thread::sleep(Duration::from_secs(60));
                    // One already-existing slot per minute, outside UI/runtime threads.
                    if root.join(slot.to_string()).is_dir()
                        && let Ok((dir, _lock)) = locked_slot(&root, slot)
                        && let Ok(entries) = std::fs::read_dir(dir)
                    {
                        for entry in entries.take(3).flatten() {
                            if entry.file_name() != ".lock"
                                && entry
                                    .metadata()
                                    .ok()
                                    .and_then(|m| m.modified().ok())
                                    .and_then(|t| SystemTime::now().duration_since(t).ok())
                                    .is_some_and(|age| age > TTL)
                            {
                                let _ = std::fs::remove_file(entry.path());
                            }
                        }
                    }
                    slot = (slot + 1) % SLOTS;
                }
            });
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounded_capture_retains_tail_and_counts_discarded_bytes() {
        let mut capture = Capture::default();
        capture.push(&vec![b'x'; CAPTURE_BYTES + 100]);
        capture.push(b"\nlast line\n");
        assert_eq!(capture.prefix.len(), CAPTURE_BYTES);
        assert!(capture.tail.ends_with(b"last line\n"));
        assert_eq!(capture.bytes, (CAPTURE_BYTES + 111) as u64);
    }
    #[test]
    fn rotating_slots_bound_disk_and_expire_unique_paths() {
        let root = tempfile::tempdir().unwrap();
        let store = OutputStore {
            next: AtomicUsize::new(0),
        };
        let (first, _) = store.save(root.path(), "first").unwrap();
        for _ in 0..SLOTS {
            store.save(root.path(), "new").unwrap();
        }
        assert!(!first.exists());
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), SLOTS);
        for dir in std::fs::read_dir(root.path()).unwrap() {
            assert_eq!(std::fs::read_dir(dir.unwrap().path()).unwrap().count(), 2);
        }
    }
    #[test]
    fn preview_respects_unicode_and_line_limits() {
        let text = "日本語\n".repeat(5000);
        let preview = tail_preview(&text);
        assert!(preview.len() <= PREVIEW_BYTES);
        assert!(preview.lines().count() <= PREVIEW_LINES);
    }
}
