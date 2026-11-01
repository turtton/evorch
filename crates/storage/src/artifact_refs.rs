//! The reference table of artifact blobs (`artifact_refs`) and the state of the
//! blob collector (`gc_state`, `gc_unknown_meta`).
//!
//! A blob (`blobs/<sha256>.<ext>`) is content addressed and may be shared by the
//! meta files of several threads, so it can only be removed when *no* meta file
//! names its SHA-256. `artifact_refs` mirrors the meta files: one row per
//! artifact, written **before** the meta file is published. The collector
//! (`runtime::artifacts::gc`) runs while the exclusive startup locks are held and
//! never treats an error, an unreadable file or a missing row as "unreferenced".
//!
//! The writer API ([`StorageHandle::record_artifact_ref`],
//! [`StorageHandle::remove_artifact_refs`]) is for normal operation. The
//! [`Database`] methods write through their own connection and are for the
//! exclusive startup phase, where the caller holds both leases.

use crate::{Database, StorageError};

/// One artifact's claim on a blob.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactRef {
    /// `artifact-<32 hex>`, the meta file's stem.
    pub artifact_id: String,
    /// Lower-case hex SHA-256 of the blob.
    pub sha256: String,
    pub thread_id: Option<String>,
    pub run_id: Option<String>,
    pub recorded_at_ns: i64,
}

/// Where the collector stands between starts.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GcState {
    /// Not written any more; kept for the schema (see migration v37).
    pub meta_cursor: Option<String>,
    /// The last blob name examined by the sweep (`None`: start from the beginning).
    pub blob_cursor: Option<String>,
    /// The last artifact ID whose meta file was checked for the orphan-reference check.
    pub ref_cursor: Option<String>,
    /// Whether the latest start proved that every meta file has a reference.
    pub refs_complete: bool,
}

/// What the reference tables know about one meta file name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetaState {
    /// A reference row exists.
    Referenced,
    /// The file was tried and could not be read; it blocks the sweep.
    Unknown,
    /// Neither.
    Missing,
}

/// A meta file whose referenced blob could not be determined.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownMeta {
    /// File name, `artifact-<32 hex>.json`.
    pub name: String,
    pub first_seen_ns: i64,
    pub last_error: String,
}

impl Database {
    /// The collector's state.
    ///
    /// # Errors
    /// Returns an error if SQLite access fails.
    pub fn gc_state(&self) -> Result<GcState, StorageError> {
        crate::repo::artifact_refs::gc_state(&self.conn)
    }

    /// Replace the collector's state.
    ///
    /// # Errors
    /// Returns an error if SQLite access fails.
    pub fn set_gc_state(&self, state: &GcState) -> Result<(), StorageError> {
        crate::repo::artifact_refs::set_gc_state(&self.conn, state)
    }

    /// Whether `artifact_id` has a reference row.
    ///
    /// # Errors
    /// Returns an error if SQLite access fails.
    pub fn artifact_ref_exists(&self, artifact_id: &str) -> Result<bool, StorageError> {
        crate::repo::artifact_refs::ref_exists(&self.conn, artifact_id)
    }

    /// Whether any reference row names the blob `sha256`.
    ///
    /// # Errors
    /// Returns an error if SQLite access fails. The caller must treat that as "maybe
    /// referenced", never as "not referenced".
    pub fn artifact_sha_referenced(&self, sha256: &str) -> Result<bool, StorageError> {
        crate::repo::artifact_refs::sha_referenced(&self.conn, sha256)
    }

    /// What the tables know about the meta file of `artifact_id`.
    ///
    /// # Errors
    /// Returns an error if SQLite access fails.
    pub fn artifact_meta_state(&self, artifact_id: &str) -> Result<MetaState, StorageError> {
        crate::repo::artifact_refs::meta_state(&self.conn, artifact_id)
    }

    /// Insert (or replace) reference rows in one transaction.
    ///
    /// # Errors
    /// Returns an error if SQLite access fails; nothing is inserted then.
    pub fn gc_insert_refs(&self, refs: &[ArtifactRef]) -> Result<(), StorageError> {
        crate::repo::artifact_refs::insert_refs(&self.conn, refs)
    }

    /// Delete reference rows in one transaction; how many existed.
    ///
    /// # Errors
    /// Returns an error if SQLite access fails; nothing is deleted then.
    pub fn gc_delete_refs(&self, artifact_ids: &[String]) -> Result<usize, StorageError> {
        crate::repo::artifact_refs::delete_refs(&self.conn, artifact_ids)
    }

    /// Delete one reference row; whether it existed.
    ///
    /// # Errors
    /// Returns an error if SQLite access fails.
    pub fn gc_delete_ref(&self, artifact_id: &str) -> Result<bool, StorageError> {
        crate::repo::artifact_refs::delete_refs(&self.conn, &[artifact_id.to_owned()])
            .map(|removed| removed > 0)
    }

    /// Up to `limit` reference rows with an artifact ID greater than `cursor`, in
    /// ID order.
    ///
    /// # Errors
    /// Returns an error if SQLite access fails.
    pub fn artifact_refs_after(
        &self,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<Vec<ArtifactRef>, StorageError> {
        crate::repo::artifact_refs::refs_after(&self.conn, cursor, limit)
    }

    /// Record that the meta file `name` could not be read. The first sighting's
    /// time is kept; the error is replaced.
    ///
    /// # Errors
    /// Returns an error if SQLite access fails.
    pub fn gc_unknown_meta_upsert(
        &self,
        name: &str,
        error: &str,
        now_ns: i64,
    ) -> Result<(), StorageError> {
        crate::repo::artifact_refs::unknown_upsert(&self.conn, name, error, now_ns)
    }

    /// Forget the unreadable-meta record of `name`.
    ///
    /// # Errors
    /// Returns an error if SQLite access fails.
    pub fn gc_unknown_meta_delete(&self, name: &str) -> Result<bool, StorageError> {
        crate::repo::artifact_refs::unknown_delete(&self.conn, name)
    }

    /// Up to `limit` unreadable-meta records in name order.
    ///
    /// # Errors
    /// Returns an error if SQLite access fails.
    pub fn gc_unknown_meta_list(&self, limit: usize) -> Result<Vec<UnknownMeta>, StorageError> {
        crate::repo::artifact_refs::unknown_list(&self.conn, limit)
    }

    /// How many meta files are recorded as unreadable.
    ///
    /// # Errors
    /// Returns an error if SQLite access fails.
    pub fn gc_unknown_meta_count(&self) -> Result<u64, StorageError> {
        crate::repo::artifact_refs::unknown_count(&self.conn)
    }
}
