//! Bounded tool output and temporary, redacted artifacts.
//!
//! A fixed number of independently locked slots bounds disk use across restarts
//! and processes. Replacing a slot removes at most one bounded file. Paths carry
//! unique IDs, so an expired reference never silently reads another command's log.
//! The ring cursor is stored under the root and locked across processes: a new
//! process must not start over at slot zero and expire a freshly returned path.
use std::ffi::OsStr;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime};

use crate::ToolResult;
use secret_guard::SecretRedactor;

pub const PREVIEW_BYTES: usize = 16 * 1024;
pub const PREVIEW_LINES: usize = 300;
pub const CAPTURE_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_RESULT_IMAGES: usize = 4;
/// Largest text a `run_js` program gets whole when the model's copy was cut to a
/// preview. It is held in memory only, for the duration of the call.
pub const FULL_CONTENT_BYTES: usize = 1024 * 1024;
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
        let redactor = SecretRedactor::from_env();
        let safe = redactor.redact(&text);
        if !large {
            return ToolResult::success(safe.text);
        }
        let full = (!truncated && safe.text.len() <= FULL_CONTENT_BYTES).then(|| safe.text.clone());
        let mut result =
            artifact_result(&safe.text, &safe.text, self.bytes, !truncated, safe.count);
        if let Some(full) = full {
            result.set_whole_text(full);
        }
        result
    }
}

/// Apply the same ceiling before any event, GUI, or model receives a tool result.
pub(crate) fn limit_result(mut result: ToolResult) -> ToolResult {
    // Two small facts outlive any replacement of the detail below: run_js's own
    // summary (the interface reads it), and whether a post-edit checker warned
    // (never its text), so a caller such as run_js can still report the warning.
    let run_js = result
        .detail
        .as_ref()
        .and_then(|detail| detail.get("run_js"))
        .cloned();
    let mut shell_status = result
        .detail
        .as_ref()
        .and_then(|detail| detail.get("shell"))
        .cloned();
    let checker_outcome = result
        .detail
        .as_ref()
        .and_then(|detail| detail.get("comment_checker"))
        .and_then(|checker| checker.get("outcome"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    if result.images.len() > MAX_RESULT_IMAGES {
        let omitted = result.images.len() - MAX_RESULT_IMAGES;
        result.images.truncate(MAX_RESULT_IMAGES);
        result
            .content
            .push_str(&format!("\n[{omitted} more images omitted]"));
    }
    let mut replaced = false;
    // A result that was cut to a preview and an artifact reference before (shell) is
    // not cut again: the preview is not the output, so it must not become a new
    // artifact or a whole text, and its reference must keep pointing at the output.
    let already_cut = result
        .detail
        .as_ref()
        .is_some_and(|detail| detail.get("output_artifact").is_some());
    if result.content.len() > PREVIEW_BYTES + 2048
        || result.content.lines().count() > PREVIEW_LINES + 8
    {
        if already_cut {
            // Only a long reference can push it over the ceiling. Drop the head of the
            // preview, so the reference at its end survives.
            let kept = tail_preview(&result.content);
            if kept.len() < result.content.len() {
                // The status header was at the start: say it is gone, in the detail that
                // stays and in the copy a rebuilt detail gets.
                for status in result
                    .detail
                    .as_mut()
                    .and_then(|detail| detail.get_mut("shell"))
                    .into_iter()
                    .chain(shell_status.as_mut())
                {
                    status["header"] = serde_json::json!(false);
                }
            }
            result.content = kept.to_owned();
        } else {
            let mut capture = Capture::default();
            capture.push(result.content.as_bytes());
            let limited = capture.finish();
            if let Some(whole) = limited.whole_text() {
                result.set_whole_text(whole.to_owned());
            }
            result.content = limited.content;
            result.detail = limited.detail;
            replaced = true;
        }
    }
    // Structured output from external tools must not bypass the text ceiling.
    if result
        .detail
        .as_ref()
        .is_some_and(|value| value.to_string().len() > PREVIEW_BYTES)
    {
        result.detail =
            Some(serde_json::json!({"detail_omitted": "structured output exceeded 16 KiB"}));
        replaced = true;
    }
    if replaced {
        if let Some(run_js) = run_js {
            result.detail.get_or_insert_with(|| serde_json::json!({}))["run_js"] = run_js;
        }
        if let Some(shell_status) = shell_status {
            result.detail.get_or_insert_with(|| serde_json::json!({}))["shell"] = shell_status;
        }
        if let Some(outcome) = checker_outcome {
            result.detail.get_or_insert_with(|| serde_json::json!({}))["comment_checker"] =
                serde_json::json!({"outcome": outcome});
        }
    }
    result
}

pub(crate) fn artifact_result(
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

/// The largest length per text at which all `lengths` fit in `budget`, so
/// short texts leave their unused share to long ones. `usize::MAX` when all of
/// them fit uncut.
pub(crate) fn shared_cap(mut lengths: Vec<usize>, budget: usize) -> usize {
    lengths.sort_unstable();
    let mut remaining = budget;
    for (fitted, &length) in lengths.iter().enumerate() {
        let share = remaining / (lengths.len() - fitted);
        if length > share {
            return share;
        }
        remaining -= length;
    }
    usize::MAX
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

/// Extract a self-contained artifact reference for shell job metadata.
pub fn artifact_reference(text: &str) -> Option<&str> {
    let index = text.rfind(MARKER)?;
    text.ends_with(']').then_some(&text[index + 1..])
}

struct OutputStore;
fn store() -> &'static OutputStore {
    &OutputStore
}

fn root_from(base: Option<&OsStr>, euid: u32) -> io::Result<PathBuf> {
    let base = Path::new(
        base.filter(|value| !value.is_empty())
            .unwrap_or(OsStr::new("/var/tmp")),
    );
    if !base.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "EVORCH_OUTPUT_DIR must be an absolute path",
        ));
    }
    Ok(base.join(format!("evorch-output-{euid}")))
}

/// Host path mounted read-only into the command sandbox. Never uses TMPDIR.
/// EVORCH_OUTPUT_DIR overrides the /var/tmp base directory when nonempty.
pub fn output_root() -> io::Result<PathBuf> {
    let base = std::env::var_os("EVORCH_OUTPUT_DIR");
    let root = root_from(base.as_deref(), rustix::process::geteuid().as_raw())?;
    std::fs::create_dir_all(
        root.parent()
            .expect("output root has an absolute base directory"),
    )?;
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

fn next_slot(root: &Path) -> io::Result<(PathBuf, File)> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let mut cursor = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
        .open(root.join(".next"))?;
    let meta = cursor.metadata()?;
    if !meta.is_file()
        || meta.uid() != rustix::process::geteuid().as_raw()
        || meta.mode() & 0o077 != 0
        || meta.nlink() != 1
        || meta.len() > 1
    {
        return Err(io::Error::other("invalid temporary output cursor file"));
    }
    cursor.lock()?;
    let mut next = [0];
    let first = match cursor.read(&mut next)? {
        0 => 0,
        _ => usize::from(next[0]),
    };
    if first >= SLOTS {
        return Err(io::Error::other("invalid temporary output cursor index"));
    }
    for offset in 0..SLOTS {
        let index = (first + offset) % SLOTS;
        if let Ok((dir, lock)) = locked_slot(root, index) {
            cursor.rewind()?;
            cursor.write_all(&[((index + 1) % SLOTS) as u8])?;
            // Release the allocator before writing bytes, but keep this slot
            // locked until save_bytes finishes replacing its artifact.
            return Ok((dir, lock));
        }
    }
    Err(io::Error::other("all temporary output slots are busy"))
}

/// Store bytes such as an image in a temporary slot; it can expire like logs.
pub fn save_file(bytes: &[u8], extension: &str) -> io::Result<PathBuf> {
    let root = output_root()?;
    store()
        .save_bytes(&root, bytes, extension)
        .map(|(path, _)| path)
}

impl OutputStore {
    fn save(&self, root: &Path, text: &str) -> io::Result<(PathBuf, usize)> {
        let mut end = text.len().min(CAPTURE_BYTES);
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        self.save_bytes(root, &text.as_bytes()[..end], "txt")
    }

    fn save_bytes(
        &self,
        root: &Path,
        bytes: &[u8],
        extension: &str,
    ) -> io::Result<(PathBuf, usize)> {
        use std::os::unix::fs::OpenOptionsExt;
        let (dir, _lock) = next_slot(root)?;
        // Each slot contains one artifact and its lock; no recursive deletion.
        for entry in std::fs::read_dir(&dir)?.take(3) {
            let entry = entry?;
            if entry.file_name() != ".lock" {
                std::fs::remove_file(entry.path())?;
            }
        }
        let path = dir.join(format!("{}.{extension}", uuid::Uuid::new_v4()));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?;
        let end = bytes.len().min(CAPTURE_BYTES);
        file.write_all(&bytes[..end])?;
        Ok((path, end))
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
    fn trimming_the_start_of_a_cut_shell_result_records_that_its_header_is_gone() {
        let status =
            |header| serde_json::json!({"exit_code": 1, "timed_out_ms": null, "header": header});
        let long_note = format!(
            "\n[Output artifact: /var/tmp/o/{}.txt; captured 9 of 9 bytes]",
            "p".repeat(30_000)
        );
        let result = |content: String| {
            ToolResult::error(content).with_detail(serde_json::json!({
                "output_artifact": {"path": "/var/tmp/o/4.txt", "original_bytes": 9},
                "shell": status(true),
            }))
        };

        let trimmed = limit_result(result(format!("exit_code: 1\ntail{long_note}")));
        assert!(
            !trimmed.content.starts_with("exit_code: 1"),
            "the header went with the head"
        );
        assert_eq!(trimmed.detail.as_ref().unwrap()["shell"]["header"], false);

        let kept = limit_result(result(
            "exit_code: 1\ntail\n[Output artifact: /var/tmp/o/4.txt]".into(),
        ));
        assert!(kept.content.starts_with("exit_code: 1"));
        assert_eq!(kept.detail.as_ref().unwrap()["shell"]["header"], true);
    }

    #[test]
    fn a_checker_warning_outlives_the_artifact_but_its_text_does_not() {
        let detail = serde_json::json!({"comment_checker": {"outcome": "warning", "message": "UNTRUSTED TEXT"}});
        for result in [
            ToolResult::success("line\n".repeat(20_000)).with_detail(detail.clone()),
            ToolResult::success("short").with_detail(serde_json::json!({
                "comment_checker": {"outcome": "warning", "message": "UNTRUSTED TEXT"},
                "diff": "x".repeat(20_000),
            })),
        ] {
            let limited = limit_result(result);
            let detail = limited.detail.unwrap();
            assert_eq!(
                detail["comment_checker"],
                serde_json::json!({"outcome": "warning"})
            );
            assert!(!detail.to_string().contains("UNTRUSTED"));
        }
    }

    #[test]
    fn a_large_run_js_result_keeps_its_summary_next_to_the_artifact() {
        let content = "line\n".repeat(20_000);
        let result = ToolResult::success(content)
            .with_detail(serde_json::json!({"run_js": {"calls": [{"seq": 1, "name": "read"}]}}));

        let limited = limit_result(result);

        let detail = limited.detail.unwrap();
        assert!(detail["output_artifact"]["path"].is_string());
        assert_eq!(detail["run_js"]["calls"][0]["name"], "read");
    }

    #[test]
    fn root_defaults_to_var_tmp() {
        assert_eq!(
            root_from(None, 1000).unwrap(),
            PathBuf::from("/var/tmp/evorch-output-1000")
        );
    }

    #[test]
    fn root_uses_output_directory_override() {
        assert_eq!(
            root_from(Some(OsStr::new("/custom/output")), 1000).unwrap(),
            PathBuf::from("/custom/output/evorch-output-1000")
        );
    }

    #[test]
    fn root_requires_an_absolute_base() {
        for base in ["relative", ".", "../output", "~/output", " "] {
            let error = root_from(Some(OsStr::new(base)), 1000).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
            assert!(error.to_string().contains("EVORCH_OUTPUT_DIR"));
        }
    }

    #[test]
    fn root_defaults_to_var_tmp_for_empty_override() {
        assert_eq!(
            root_from(Some(OsStr::new("")), 1000).unwrap(),
            root_from(None, 1000).unwrap()
        );
    }

    #[test]
    fn root_preserves_base_and_uid_child() {
        for base in ["/", "/custom/output/", "/custom/with spaces"] {
            for uid in [0, 42, u32::MAX] {
                let root = root_from(Some(OsStr::new(base)), uid).unwrap();
                assert_eq!(root.parent(), Some(Path::new(base)));
                assert_eq!(
                    root.file_name().unwrap(),
                    format!("evorch-output-{uid}").as_str()
                );
            }
        }
    }

    #[test]
    fn root_preserves_non_utf8_absolute_base() {
        use std::os::unix::ffi::OsStrExt;
        let base = OsStr::from_bytes(b"/custom/\xff");
        assert_eq!(
            root_from(Some(base), 42).unwrap(),
            Path::new(base).join("evorch-output-42")
        );
    }

    #[test]
    fn custom_root_is_private_and_owned_by_current_user() {
        use std::os::unix::fs::MetadataExt;
        let base = tempfile::tempdir().unwrap();
        let uid = rustix::process::geteuid().as_raw();
        let root = root_from(Some(base.path().as_os_str()), uid).unwrap();
        private_dir(&root).unwrap();
        let meta = std::fs::symlink_metadata(&root).unwrap();
        assert!(meta.is_dir());
        assert_eq!(meta.uid(), uid);
        assert_eq!(meta.mode() & 0o777, 0o700);
        private_dir(&root).unwrap();
    }

    #[test]
    fn custom_root_rejects_non_private_directory() {
        use std::os::unix::fs::PermissionsExt;
        let base = tempfile::tempdir().unwrap();
        let root = root_from(
            Some(base.path().as_os_str()),
            rustix::process::geteuid().as_raw(),
        )
        .unwrap();
        private_dir(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(private_dir(&root).is_err());
    }

    #[test]
    fn custom_root_rejects_symlink() {
        let base = tempfile::tempdir().unwrap();
        let root = root_from(
            Some(base.path().as_os_str()),
            rustix::process::geteuid().as_raw(),
        )
        .unwrap();
        std::os::unix::fs::symlink(base.path(), &root).unwrap();
        assert!(private_dir(&root).is_err());
    }

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
    fn independent_processes_retain_artifacts_until_the_shared_ring_wraps() {
        const CHILD: &str = "EVORCH_OUTPUT_RING_CHILD";
        const TEST: &str =
            "output::tests::independent_processes_retain_artifacts_until_the_shared_ring_wraps";
        if let Some(label) = std::env::var_os(CHILD) {
            let path = save_file(label.as_encoded_bytes(), "txt").unwrap();
            let base = PathBuf::from(std::env::var_os("EVORCH_OUTPUT_DIR").unwrap());
            std::fs::write(base.join(label), path.as_os_str().as_encoded_bytes()).unwrap();
            return;
        }

        let base = tempfile::tempdir().unwrap();
        let save_in_new_process = |label: &str| {
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", TEST, "--nocapture"])
                .env(CHILD, label)
                .env("EVORCH_OUTPUT_DIR", base.path())
                .status()
                .unwrap();
            assert!(status.success(), "writer {label} failed");
            PathBuf::from(std::fs::read_to_string(base.path().join(label)).unwrap())
        };

        // Force save(A) -> save(B) -> read(A), without timing or scheduler
        // assumptions. A process-local cursor starts at slot zero every time.
        let first = save_in_new_process("first");
        let second = save_in_new_process("second");
        assert_eq!(std::fs::read_to_string(&first).unwrap(), "first");
        assert_eq!(std::fs::read_to_string(&second).unwrap(), "second");
        let root = first.parent().unwrap().parent().unwrap();
        for _ in 2..SLOTS {
            store().save(root, "filler").unwrap();
        }
        assert_eq!(std::fs::read_to_string(&first).unwrap(), "first");
        save_in_new_process("wrap");
        assert!(!first.exists(), "the oldest reference expires at capacity");
        assert_eq!(std::fs::read_to_string(&second).unwrap(), "second");
    }

    #[test]
    fn concurrent_processes_allocate_distinct_slots() {
        const CHILD: &str = "EVORCH_OUTPUT_CONCURRENT_CHILD";
        const TEST: &str = "output::tests::concurrent_processes_allocate_distinct_slots";
        if let Ok(label) = std::env::var(CHILD) {
            // A pipe gates all writers until the parent has spawned them.
            std::io::stdin().read_exact(&mut [0]).unwrap();
            let path = save_file(label.as_bytes(), "txt").unwrap();
            let base = PathBuf::from(std::env::var_os("EVORCH_OUTPUT_DIR").unwrap());
            std::fs::write(base.join(label), path.as_os_str().as_encoded_bytes()).unwrap();
            return;
        }
        let base = tempfile::tempdir().unwrap();
        let mut children = (0..8)
            .map(|index| {
                std::process::Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", TEST, "--nocapture"])
                    .env(CHILD, index.to_string())
                    .env("EVORCH_OUTPUT_DIR", base.path())
                    .stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::null())
                    .spawn()
                    .unwrap()
            })
            .collect::<Vec<_>>();
        for child in &mut children {
            child.stdin.take().unwrap().write_all(b"x").unwrap();
        }
        for mut child in children {
            assert!(child.wait().unwrap().success(), "writer failed");
        }
        let mut slots = std::collections::HashSet::new();
        for index in 0..8 {
            let label = index.to_string();
            let path = PathBuf::from(std::fs::read_to_string(base.path().join(&label)).unwrap());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), label);
            assert!(slots.insert(path.parent().unwrap().to_owned()));
        }
    }

    #[test]
    fn allocation_skips_busy_slots_and_fails_without_expiring_them_when_full() {
        let root = tempfile::tempdir().unwrap();
        let (dir, lock) = locked_slot(root.path(), 0).unwrap();
        let held = dir.join("held.txt");
        std::fs::write(&held, "held").unwrap();
        let (saved, _) = store().save(root.path(), "saved").unwrap();
        assert_ne!(saved.parent().unwrap(), dir);
        assert_eq!(std::fs::read_to_string(&held).unwrap(), "held");

        let mut locks = vec![lock];
        for index in 1..SLOTS {
            locks.push(locked_slot(root.path(), index).unwrap().1);
        }
        assert!(store().save(root.path(), "no room").is_err());
        assert_eq!(std::fs::read_to_string(&held).unwrap(), "held");
        assert_eq!(std::fs::read_to_string(&saved).unwrap(), "saved");
        drop(locks);
        assert!(store().save(root.path(), "unlocked").is_ok());
    }

    #[test]
    fn cursor_rejects_symlinks_hardlinks_public_files_and_invalid_data() {
        use std::os::unix::fs::PermissionsExt;
        for case in [
            "symlink",
            "hardlink",
            "directory",
            "public",
            "oversized",
            "index",
        ] {
            let root = tempfile::tempdir().unwrap();
            let target = tempfile::NamedTempFile::new().unwrap();
            std::fs::write(target.path(), [0]).unwrap();
            let cursor = root.path().join(".next");
            match case {
                "symlink" => std::os::unix::fs::symlink(target.path(), &cursor).unwrap(),
                "hardlink" => std::fs::hard_link(target.path(), &cursor).unwrap(),
                "directory" => std::fs::create_dir(&cursor).unwrap(),
                "public" => {
                    std::fs::write(&cursor, [0]).unwrap();
                    std::fs::set_permissions(&cursor, std::fs::Permissions::from_mode(0o644))
                        .unwrap();
                }
                _ => {
                    std::fs::write(
                        &cursor,
                        if case == "index" {
                            vec![SLOTS as u8]
                        } else {
                            vec![0; 2]
                        },
                    )
                    .unwrap();
                    std::fs::set_permissions(&cursor, std::fs::Permissions::from_mode(0o600))
                        .unwrap();
                }
            }
            assert!(store().save(root.path(), "blocked").is_err(), "{case}");
            assert_eq!(std::fs::read(target.path()).unwrap(), [0], "{case}");
            assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1, "{case}");
        }
    }

    #[test]
    fn rotating_slots_bound_disk_and_expire_unique_paths() {
        let root = tempfile::tempdir().unwrap();
        let store = OutputStore;
        let (first, _) = store.save(root.path(), "first").unwrap();
        for _ in 0..SLOTS {
            store.save(root.path(), "new").unwrap();
        }
        assert!(!first.exists());
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), SLOTS + 1);
        assert_eq!(
            std::fs::metadata(root.path().join(".next")).unwrap().len(),
            1
        );
        for index in 0..SLOTS {
            assert_eq!(
                std::fs::read_dir(root.path().join(index.to_string()))
                    .unwrap()
                    .count(),
                2
            );
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
