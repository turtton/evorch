//! Free-space planning of `--apply`: what the conversion adds to the database, the log of
//! the full VACUUM, the verified backup and SQLite's scratch files are different amounts
//! that may land on different volumes, so they are summed per volume.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::StorageError;

use super::super::invalid;

/// What one volume must have free.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct VolumeNeed {
    /// A directory on the volume.
    pub directory: PathBuf,
    pub bytes: u64,
    /// What the bytes are for.
    pub purposes: Vec<&'static str>,
}

/// A directory with an identity for its volume.
pub(super) struct Place<'a> {
    pub directory: &'a Path,
    /// Equal for directories on one volume.
    pub volume: u64,
}

pub(super) fn place(directory: &Path) -> Result<Place<'_>, StorageError> {
    #[cfg(unix)]
    let volume = {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(directory)
            .map_err(|error| StorageError::Io(error.to_string()))?
            .dev()
    };
    #[cfg(not(unix))]
    let volume = 0;
    Ok(Place { directory, volume })
}

/// The needs per volume for the whole conversion.
///
/// `basis` is the database's size before it starts and `growth` an upper bound of what the
/// converted rows add. On the database's volume: the growth, then the log of the VACUUM,
/// which is as large as the new file (`basis + growth`). On the backup's volume: the
/// backup, one database. On the volume of SQLite's scratch files: the rebuilt database of
/// the VACUUM. The old rule of four databases stays the floor of the volumes that hold the
/// database or the backup.
pub(super) fn plan(
    database: &Place<'_>,
    backup: &Place<'_>,
    scratch: &Place<'_>,
    basis: u64,
    growth: u64,
) -> Vec<VolumeNeed> {
    let new_size = basis.saturating_add(growth);
    let entries: [(&Place<'_>, u64, &'static str, bool); 3] = [
        (
            database,
            growth.saturating_add(new_size),
            "growth of the database and the log of the full VACUUM",
            true,
        ),
        (backup, basis, "verified backup", true),
        (scratch, new_size, "SQLite scratch of the VACUUM", false),
    ];
    let mut needs: Vec<(u64, VolumeNeed, bool)> = Vec::new();
    for (place, bytes, purpose, floor) in entries {
        match needs
            .iter_mut()
            .find(|(volume, _, _)| *volume == place.volume)
        {
            Some((_, need, floored)) => {
                need.bytes = need.bytes.saturating_add(bytes);
                need.purposes.push(purpose);
                *floored |= floor;
            }
            None => needs.push((
                place.volume,
                VolumeNeed {
                    directory: place.directory.to_path_buf(),
                    bytes,
                    purposes: vec![purpose],
                },
                floor,
            )),
        }
    }
    needs
        .into_iter()
        .map(|(_, mut need, floored)| {
            if floored {
                need.bytes = need.bytes.max(basis.saturating_mul(4));
            }
            need
        })
        .collect()
}

/// What the VACUUM alone still needs once the rows are converted: its log on the
/// database's volume and its scratch file where SQLite puts it.
pub(super) fn plan_vacuum(database: &Place<'_>, scratch: &Place<'_>, size: u64) -> Vec<VolumeNeed> {
    let mut needs = vec![VolumeNeed {
        directory: database.directory.to_path_buf(),
        bytes: size,
        purposes: vec!["log of the full VACUUM"],
    }];
    if scratch.volume == database.volume {
        needs[0].bytes = needs[0].bytes.saturating_add(size);
        needs[0].purposes.push("SQLite scratch of the VACUUM");
    } else {
        needs.push(VolumeNeed {
            directory: scratch.directory.to_path_buf(),
            bytes: size,
            purposes: vec!["SQLite scratch of the VACUUM"],
        });
    }
    needs
}

/// Free bytes on the volume of `directory`.
pub(super) fn available(directory: &Path) -> Result<u64, StorageError> {
    #[cfg(test)]
    if let Some(bytes) = tests_override::get() {
        return Ok(bytes);
    }
    super::super::offline::available_space(directory)
}

/// Refuses when any volume lacks what it needs. A check cannot reserve space.
pub(super) fn require(needs: &[VolumeNeed]) -> Result<(), StorageError> {
    for need in needs {
        let free = available(&need.directory)?;
        if free < need.bytes {
            return Err(invalid(format!(
                "insufficient free space on the volume of {}: need {} bytes ({}), available {free}; \
                 a check cannot reserve space",
                need.directory.display(),
                need.bytes,
                need.purposes.join(" + ")
            )));
        }
    }
    Ok(())
}

/// Where SQLite puts scratch files (`SQLITE_TMPDIR`, then `TMPDIR`, else `/var/tmp`).
pub(super) fn scratch_directory() -> PathBuf {
    for name in ["SQLITE_TMPDIR", "TMPDIR"] {
        if let Some(path) = std::env::var_os(name).map(PathBuf::from)
            && path.is_dir()
        {
            return path;
        }
    }
    PathBuf::from("/var/tmp")
}

#[cfg(test)]
pub(super) mod tests_override {
    use std::cell::Cell;

    thread_local! {
        static FREE: Cell<Option<u64>> = const { Cell::new(None) };
    }

    pub(in crate::maintenance::convert_bodies) fn get() -> Option<u64> {
        FREE.with(Cell::get)
    }

    pub(in crate::maintenance::convert_bodies) fn set(bytes: Option<u64>) {
        FREE.with(|free| free.set(bytes));
    }
}
