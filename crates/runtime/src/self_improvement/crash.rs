use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

use super::{bound_text, drafts::atomic_write};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpooledCrash {
    pub file_name: String,
    pub message: String,
    pub location: Option<String>,
    pub thread: Option<String>,
    pub timestamp_unix: u64,
    /// [`super::build_info`] of the crashed process; absent in older spool entries.
    pub build: Option<String>,
    /// [`compact_backtrace`] of the panicking thread; absent in older spool entries.
    pub backtrace: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct CrashEntry {
    message: String,
    location: Option<String>,
    thread: Option<String>,
    #[serde(alias = "timestamp_unix")]
    timestamp: u64,
    #[serde(default)]
    build: Option<String>,
    #[serde(default)]
    backtrace: Option<String>,
}

/// Wraps the process panic hook: spools the panic, then runs the previous hook so
/// stderr reporting is unchanged. Panics inside [`crate::panic_capture::catch_panic`]
/// are recorded for their catcher instead of spooled. The composition owner decides whether/when to
/// install this global hook. It is not installed by the collector or builder.
/// Disk/serialization/clock/randomness failures are ignored; the spooling part never
/// unwraps, logs, or invokes user formatting.
/// Calling during a panic is ignored because std::panic::set_hook would panic.
pub fn install_crash_spool(spool_dir: PathBuf) {
    if std::thread::panicking() {
        return;
    }
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // A caught panic is reported live by its catcher; the process keeps running.
        if crate::panic_capture::record_if_caught(info) {
            previous(info);
            return;
        }
        let message = crate::panic_capture::payload_message(info.payload());
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let entry = CrashEntry {
            message: bound_text(message, 4096),
            location: info
                .location()
                .map(|l| bound_text(&format!("{}:{}:{}", l.file(), l.line(), l.column()), 4096)),
            thread: std::thread::current()
                .name()
                .map(|name| bound_text(name, 1024)),
            timestamp,
            build: Some(super::build_info()),
            backtrace: Some(bound_text(
                &compact_backtrace(&std::backtrace::Backtrace::force_capture().to_string()),
                BACKTRACE_MAX_BYTES,
            )),
        };
        if let Ok(bytes) = serde_json::to_vec(&entry)
            && fs::create_dir_all(&spool_dir).is_ok()
        {
            let path = spool_dir.join(format!("crash-{timestamp}-{}.json", std::process::id()));
            let _ = atomic_write(&path, &bytes);
        }
        previous(info);
    }));
}

const BACKTRACE_MAX_BYTES: usize = 8192;
const BACKTRACE_MAX_FRAMES: usize = 24;

/// Keeps the frames that locate a harness fault: drops the panic machinery, std,
/// and executor frames, and folds each frame onto one `symbol @ file:line` line
/// (paths shortened to the workspace-relative `crates/...` part). Frames without
/// symbols (stripped release builds) are dropped, so this can be empty.
pub(crate) fn compact_backtrace(rendered: &str) -> String {
    const NOISE: &[&str] = &[
        "std::",
        "core::",
        "alloc::",
        "<std::",
        "<core::",
        "<alloc::",
        "tokio::",
        "<tokio::",
        "rust_begin_unwind",
        "__rust",
        "__libc",
        "_start",
        "<unknown>",
        "runtime::self_improvement::crash::",
    ];
    let mut frames: Vec<String> = Vec::new();
    let mut keep = false;
    for line in rendered.lines() {
        let trimmed = line.trim();
        if let Some(location) = trimmed.strip_prefix("at ") {
            if keep && let Some(frame) = frames.last_mut() {
                let location = location
                    .find("crates/")
                    .map_or(location, |i| &location[i..]);
                frame.push_str(" @ ");
                frame.push_str(location);
            }
            continue;
        }
        let Some((index, symbol)) = trimmed.split_once(": ") else {
            continue;
        };
        if index.parse::<usize>().is_err() {
            continue;
        }
        keep = !NOISE.iter().any(|noise| symbol.starts_with(noise));
        if keep {
            frames.push(symbol.to_string());
        }
    }
    let total = frames.len();
    frames.truncate(BACKTRACE_MAX_FRAMES);
    if total > BACKTRACE_MAX_FRAMES {
        frames.push(format!("… {} more frames", total - BACKTRACE_MAX_FRAMES));
    }
    frames.join("\n")
}

/// Best-effort, deterministic read-and-delete of crash-*.json only. Missing dirs
/// return empty; unreadable/corrupt/oversized entries become explicit evidence.
/// Symlinks are not followed. Failed deletion is ignored (re-intake folds into the
/// same candidate as another occurrence).
pub fn drain_crash_spool(spool_dir: &Path) -> Vec<SpooledCrash> {
    let Ok(entries) = fs::read_dir(spool_dir) else {
        return Vec::new();
    };
    let mut paths: Vec<_> = entries
        .flatten()
        .filter(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            name.starts_with("crash-") && name.ends_with(".json")
        })
        .collect();
    paths.sort_by_key(|entry| entry.file_name());
    paths
        .into_iter()
        .filter_map(|entry| {
            // Leave actual directories alone, even if they have matching names.
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                return None;
            }
            let mut bytes = Vec::new();
            let parsed = if entry.file_type().is_ok_and(|kind| kind.is_file()) {
                fs::File::open(entry.path())
                    .ok()
                    .and_then(|file| file.take(1_048_577).read_to_end(&mut bytes).ok())
                    .filter(|size| *size <= 1_048_576)
                    .and_then(|_| serde_json::from_slice::<CrashEntry>(&bytes).ok())
            } else {
                None
            };
            let _ = fs::remove_file(entry.path());
            let parsed = parsed.unwrap_or(CrashEntry {
                message: "<unreadable spool entry>".into(),
                location: None,
                thread: None,
                timestamp: 0,
                build: None,
                backtrace: None,
            });
            Some(SpooledCrash {
                file_name: entry.file_name().to_string_lossy().into_owned(),
                message: parsed.message,
                location: parsed.location,
                thread: parsed.thread,
                timestamp_unix: parsed.timestamp,
                build: parsed.build,
                backtrace: parsed.backtrace,
            })
        })
        .collect()
}
