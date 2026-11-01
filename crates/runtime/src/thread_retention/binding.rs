//! The two-way binding between a database and its ownership root.
//!
//! Deleting a family also deletes state that lives outside the database: owner
//! rows and scratch directories under the ownership root. Doing that for a root
//! that belongs to another database (or after the sidebar was swapped for another
//! file) would destroy live state, so the first confirmation records the pairing on
//! both sides and every later one checks it:
//!
//! * database side: `retention_identity.db_id` and the `retention_binding` row
//!   (ownership root and sidebar path), written by `retention_finalize`;
//! * root side: `<ownership_root>/.evorch-retention-binding.json`, written before
//!   the first finalize and never overwritten.
//!
//! A mismatch disables new confirmations and every outside deletion. There is no
//! rebind operation.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use storage::Database;

use super::RetentionError;

pub const ROOT_BINDING_FILE: &str = ".evorch-retention-binding.json";

/// The paths a binding pairs, as given by the caller.
#[derive(Debug, Clone, Copy)]
pub struct BindingPaths<'a> {
    pub ownership_root: &'a Path,
    pub sidebar_path: &'a Path,
}

/// Where the pairing stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindingStatus {
    /// Neither side records a pairing (or only this database's own interrupted
    /// first attempt exists): this startup may establish it.
    Unbound,
    Bound,
    /// The pairing exists and does not match this start. The reason is for display.
    Mismatch(String),
}

#[derive(Debug, Serialize, Deserialize)]
struct RootBinding {
    db_id: String,
    db_path: String,
    bound_at_ns: i64,
}

/// The absolute path with symlinks resolved. A missing final component (a sidebar
/// that was never saved) resolves through its parent.
#[must_use]
pub fn canonical_string(path: &Path) -> String {
    let resolved = path.canonicalize().unwrap_or_else(|_| {
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        parent
            .canonicalize()
            .map(|parent| parent.join(path.file_name().unwrap_or_default()))
            .unwrap_or_else(|_| path.to_path_buf())
    });
    resolved.to_string_lossy().into_owned()
}

fn root_file(ownership_root: &Path) -> PathBuf {
    ownership_root.join(ROOT_BINDING_FILE)
}

fn read_root(ownership_root: &Path) -> Result<Option<RootBinding>, String> {
    match fs::read(root_file(ownership_root)) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|error| format!("root binding file is unreadable: {error}")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("root binding file is unreadable: {error}")),
    }
}

/// Compare both sides.
///
/// A root file that names this database while the database has no binding row is the
/// remainder of an interrupted (or fruitless) first attempt: the file is written before
/// the first confirmation, so `Unbound` is reported and the attempt may resume.
///
/// # Errors
/// Fails when the database cannot be read.
pub fn check_binding(
    database: &Database,
    paths: BindingPaths<'_>,
) -> Result<BindingStatus, RetentionError> {
    let db_id = database.retention_identity()?;
    let row = database.retention_binding()?;
    let file = match read_root(paths.ownership_root) {
        Ok(file) => file,
        Err(reason) => return Ok(BindingStatus::Mismatch(reason)),
    };
    let Some(file) = file else {
        return Ok(if row.is_some() {
            BindingStatus::Mismatch("root binding file is missing".into())
        } else {
            BindingStatus::Unbound
        });
    };
    if file.db_id != db_id {
        return Ok(BindingStatus::Mismatch(
            "ownership root is bound to another database".into(),
        ));
    }
    let Some(row) = row else {
        return Ok(BindingStatus::Unbound);
    };
    if row.ownership_root != canonical_string(paths.ownership_root) {
        return Ok(BindingStatus::Mismatch(
            "database is bound to another ownership root".into(),
        ));
    }
    if row.sidebar_path != canonical_string(paths.sidebar_path) {
        return Ok(BindingStatus::Mismatch(
            "database is bound to another sidebar file".into(),
        ));
    }
    Ok(BindingStatus::Bound)
}

/// Write the root side of the binding unless it exists. An existing file must name
/// this database. The file appears whole or not at all (written aside, then linked
/// into place without replacing).
///
/// # Errors
/// Fails when the file cannot be written or names another database.
pub fn write_root_binding(
    ownership_root: &Path,
    db_id: &str,
    db_path: &Path,
    now_ns: i64,
) -> Result<(), RetentionError> {
    let target = root_file(ownership_root);
    let body = serde_json::to_vec(&RootBinding {
        db_id: db_id.to_owned(),
        db_path: canonical_string(db_path),
        bound_at_ns: now_ns,
    })
    .map_err(|error| RetentionError::Binding(error.to_string()))?;
    let temporary = ownership_root.join(format!("{ROOT_BINDING_FILE}.tmp-{}", std::process::id()));
    let written = (|| {
        let mut file = fs::File::create(&temporary)?;
        file.write_all(&body)?;
        file.sync_all()
    })();
    if let Err(error) = written {
        let _ = fs::remove_file(&temporary);
        return Err(error.into());
    }
    let linked = fs::hard_link(&temporary, &target);
    let _ = fs::remove_file(&temporary);
    match linked {
        Ok(()) => {
            // Durable before the database may record the pairing.
            fs::File::open(ownership_root).and_then(|directory| directory.sync_all())?;
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            match read_root(ownership_root) {
                Ok(Some(existing)) if existing.db_id == db_id => Ok(()),
                _ => Err(RetentionError::Binding(
                    "ownership root is bound to another database".into(),
                )),
            }
        }
        Err(error) => Err(error.into()),
    }
}
