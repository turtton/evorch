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
}

#[derive(Serialize, Deserialize)]
struct CrashEntry {
    message: String,
    location: Option<String>,
    thread: Option<String>,
    #[serde(alias = "timestamp_unix")]
    timestamp: u64,
}

/// Replaces the process panic hook; deliberately does NOT chain to the old hook.
/// The composition owner decides whether/when to install this global hook. It is
/// not installed by the collector or builder. Disk/serialization/clock/randomness
/// failures are ignored; the hook never unwraps, logs, or invokes user formatting.
/// Calling during a panic is ignored because std::panic::set_hook would panic.
pub fn install_crash_spool(spool_dir: PathBuf) {
    if std::thread::panicking() {
        return;
    }
    std::panic::set_hook(Box::new(move |info| {
        let message = if let Some(s) = info.payload().downcast_ref::<&str>() {
            *s
        } else if let Some(s) = info.payload().downcast_ref::<String>() {
            s.as_str()
        } else {
            "<non-string panic payload>"
        };
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
        };
        if let Ok(bytes) = serde_json::to_vec(&entry)
            && fs::create_dir_all(&spool_dir).is_ok()
        {
            let path = spool_dir.join(format!("crash-{timestamp}-{}.json", std::process::id()));
            let _ = atomic_write(&path, &bytes);
        }
    }));
}

/// Best-effort, deterministic read-and-delete of crash-*.json only. Missing dirs
/// return empty; unreadable/corrupt/oversized entries become explicit evidence.
/// Symlinks are not followed. Failed deletion is ignored (dedup handles re-intake).
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
            });
            Some(SpooledCrash {
                file_name: entry.file_name().to_string_lossy().into_owned(),
                message: parsed.message,
                location: parsed.location,
                thread: parsed.thread,
                timestamp_unix: parsed.timestamp,
            })
        })
        .collect()
}
