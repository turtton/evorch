//! The verified backup that must exist before the cleanup changes anything.
//!
//! The backup is a byte copy of the checkpointed main database file, taken while the caller
//! holds every exclusion, so it is exactly the image the plan names. It is published without
//! overwriting anything and verified before the source is touched.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

use super::super::invalid;
use super::super::offline::{read_only_preview_connection, require_space};
use super::{hex, sha256_file};
use crate::StorageError;

/// Headroom for the WAL and the cleanup's own pages beyond the image itself.
const HEADROOM_BYTES: u64 = 64 * 1024 * 1024;

pub(super) struct Backup {
    pub path: PathBuf,
    pub sha256: String,
    pub reused: bool,
}

fn io(error: std::io::Error) -> StorageError {
    StorageError::Io(error.to_string())
}

fn parent_of(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
}

/// Refuses a size problem or an unusable target before any byte is written.
fn target_path(source: &Path, backup: &Path) -> Result<PathBuf, StorageError> {
    let name = backup
        .file_name()
        .ok_or_else(|| invalid("backup path has no file name"))?;
    let parent = parent_of(backup).canonicalize().map_err(io)?;
    if !fs::metadata(&parent).map_err(io)?.is_dir() {
        return Err(invalid("backup parent is not a directory"));
    }
    let target = parent.join(name);
    let sidecars = [
        source.to_path_buf(),
        crate::db::suffixed_path(source, "-wal"),
        crate::db::suffixed_path(source, "-shm"),
    ];
    if sidecars.contains(&target) {
        return Err(invalid("backup path is the database or one of its files"));
    }
    Ok(target)
}

#[cfg(unix)]
fn same_file(a: &fs::Metadata, b: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    a.dev() == b.dev() && a.ino() == b.ino()
}

#[cfg(not(unix))]
fn same_file(_: &fs::Metadata, _: &fs::Metadata) -> bool {
    true
}

fn quick_check(path: &Path) -> Result<(), StorageError> {
    // immutable: the check never creates a WAL or shared-memory file beside the backup.
    let (conn, files) = read_only_preview_connection(path)?;
    let verdict: String = conn.query_row("PRAGMA quick_check", [], |row| row.get(0))?;
    if verdict != "ok" {
        return Err(invalid("backup integrity check failed"));
    }
    files.verify()
}

/// Makes sure `backup` holds a verified byte copy of `source` whose hash is `expected`.
/// An existing file is reused only when it is that exact image; nothing is overwritten.
///
/// # Errors
/// The target aliases the source, an existing file differs or is damaged, the volume is too
/// small, or any write or verification step fails (the source is untouched either way).
pub(super) fn ensure_backup(
    source: &Path,
    expected: &str,
    backup: &Path,
) -> Result<Backup, StorageError> {
    let target = target_path(source, backup)?;
    let source_meta = fs::metadata(source).map_err(io)?;
    match fs::symlink_metadata(&target) {
        Ok(meta) => {
            if meta.file_type().is_symlink() || !meta.is_file() {
                return Err(invalid("backup path exists and is not a regular file"));
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                if meta.nlink() != 1 {
                    return Err(invalid("backup file has other hard links"));
                }
            }
            if same_file(&meta, &source_meta) {
                return Err(invalid("backup path is the database file"));
            }
            if sha256_file(&target)? != expected {
                return Err(invalid(
                    "backup path holds a different image than this plan's source; choose a new path",
                ));
            }
            quick_check(&target)?;
            return Ok(Backup {
                path: target,
                sha256: expected.to_owned(),
                reused: true,
            });
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(io(error)),
    }
    let parent = parent_of(&target).to_path_buf();
    let size = source_meta.len();
    require_space(&parent, size.saturating_add(HEADROOM_BYTES))?;
    let source_parent = parent_of(source);
    require_space(source_parent, HEADROOM_BYTES)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if fs::metadata(&parent).map_err(io)?.dev()
            == fs::metadata(source_parent).map_err(io)?.dev()
        {
            require_space(source_parent, size.saturating_add(2 * HEADROOM_BYTES))?;
        }
    }
    let ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let mut temporary = target.clone().into_os_string();
    temporary.push(format!(".tmp-{}-{ns}", std::process::id()));
    let temporary = PathBuf::from(temporary);
    let result = copy_and_publish(source, expected, &temporary, &target, &parent);
    let _ = fs::remove_file(&temporary);
    result?;
    Ok(Backup {
        path: target,
        sha256: expected.to_owned(),
        reused: false,
    })
}

fn copy_and_publish(
    source: &Path,
    expected: &str,
    temporary: &Path,
    target: &Path,
    parent: &Path,
) -> Result<(), StorageError> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut out = options.open(temporary).map_err(io)?;
    let mut input = File::open(source).map_err(io)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1 << 20];
    loop {
        let read = input.read(&mut buffer).map_err(io)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        out.write_all(&buffer[..read]).map_err(io)?;
    }
    out.sync_all().map_err(io)?;
    drop(out);
    if hex(&hasher.finalize()) != expected {
        return Err(invalid(
            "the database changed while it was copied; nothing was published",
        ));
    }
    if sha256_file(temporary)? != expected {
        return Err(invalid("backup copy does not match the source image"));
    }
    quick_check(temporary)?;
    // A hard link fails when the name exists, so a backup is never replaced.
    fs::hard_link(temporary, target).map_err(|error| {
        invalid(format!(
            "backup could not be published without overwriting ({error})"
        ))
    })?;
    File::open(parent)
        .and_then(|dir| dir.sync_all())
        .map_err(io)?;
    Ok(())
}
