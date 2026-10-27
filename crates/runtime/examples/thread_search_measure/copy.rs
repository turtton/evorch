use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OpenFlags, backup::Backup};

use super::{Result, require};

/// Both backup sources and generated seeds are read-only. Never open the live
/// store, use filesystem copies of a WAL database, or reuse an existing target.
pub fn backup(source: &Path, destination: &Path) -> Result<()> {
    let parent = destination
        .parent()
        .ok_or("backup_parent_missing")?
        .canonicalize()?;
    let identity = parent.join(destination.file_name().ok_or("backup_filename_missing")?);
    require(source.canonicalize()? != identity, "backup_alias")?;
    let _reservation = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(destination, std::fs::Permissions::from_mode(0o600))?;
    }
    let source = Connection::open_with_flags(
        source,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    require(source.is_readonly("main")?, "source_not_readonly")?;
    let mut destination = Connection::open(destination)?;
    let backup = Backup::new(&source, &mut destination)?;
    backup.run_to_completion(256, Duration::from_millis(1), None)?;
    Ok(())
}
